use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use shenko_evidence::{
    EvidenceRecord, IndexError, LongRunInput, ReportInputs, Scope, SpecIndex, UnmetReason, analyze,
    build_report, summarize,
};

mod support;

use support::{BINARY, CANDIDATE, sealed_run_report};

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn bound_file(root: &Path, name: &str, contents: &[u8]) -> Value {
    fs::write(root.join(name), contents).unwrap();
    json!({"path": name, "sha256": digest(contents)})
}

fn spec(case_id: &str) -> Result<SpecIndex, IndexError> {
    SpecIndex::parse("fixture.md", &format!(
        "<a id=\"fixture\"></a>\n| R001 | fixture | §23 | U01;B01 |\n| B01 fixture | R001 | contract | {case_id} |\n| {case_id} | scenario | condition |\n"
    ))
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
fn range_holes_remain_unresolved_instead_of_shrinking_the_obligation() -> Result<(), IndexError> {
    let index = SpecIndex::parse("range.md", "<a id=\"range\"></a>\n| R001 | range | §45 | U01;B01 |\n| B01 range | R001 | contract | AW01–03 |\n| AW01 | first | condition |\n| AW03 | last | condition |\n")?;
    assert_eq!(index.behaviors["B01"].acceptance_cases, ["AW01", "AW03"]);
    assert_eq!(index.behaviors["B01"].unresolved_case_refs, ["AW01–03"]);
    let report = build_report(&index, &[], Some(CANDIDATE));
    assert!(
        report.requirements[0]
            .unmet_reasons
            .contains(&UnmetReason::UnknownReference)
    );
    Ok(())
}

#[test]
fn qualified_ids_cannot_replace_parent_definitions() -> Result<(), IndexError> {
    let index = SpecIndex::parse("qualified.md", "<a id=\"qualified\"></a>\n| B15 memory | R001 | contract | A01 |\n| B15.Q1 detail | upstream | comparison |\n| A01 | parent scenario | parent condition |\n| A01.detail | child scenario | child condition |\n")?;
    assert_eq!(index.behaviors["B15"].acceptance_cases, ["A01"]);
    assert_eq!(
        index.acceptance_cases["A01"].pass_condition,
        "parent condition"
    );
    assert!(matches!(
        SpecIndex::parse("duplicate.md", "<a id=\"duplicate\"></a>\n| A01 | first | condition |\n| A01 | second | different condition |\n"),
        Err(shenko_evidence::IndexError::DuplicateDefinition { kind: "acceptance case", id, .. }) if id == "A01"
    ));
    Ok(())
}

#[test]
fn unknown_case_family_stays_visible_as_unclassified() -> Result<(), IndexError> {
    let index = SpecIndex::parse("unknown-family.md", "<a id=\"unknown-family\"></a>\n| R001 | unknown family | §23 | B01 |\n| B01 extension | R001 | contract | ZZ01 |\n\n| Case | scenario | result |\n| --- | --- | --- |\n| ZZ01 | new family | condition |\n")?;
    let report = build_report(&index, &[], Some(CANDIDATE));
    assert_eq!(report.requirements[0].acceptance_cases, ["ZZ01"]);
    assert!(
        report.requirements[0]
            .unmet_reasons
            .contains(&UnmetReason::UnclassifiedScope)
    );
    Ok(())
}

#[test]
fn modifying_observed_bytes_withdraws_previously_admitted_coverage() -> Result<(), IndexError> {
    let root = tempfile::tempdir().unwrap();
    let receipt = fixture(root.path(), "A01");
    let record = publish(root.path(), &receipt);
    let index = spec("A01")?;
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
    Ok(())
}

#[test]
fn original_outcome_contradiction_cannot_be_overridden_by_the_record() -> Result<(), IndexError> {
    let root = tempfile::tempdir().unwrap();
    let mut receipt = fixture(root.path(), "A01");
    receipt["passed"] = json!(false);
    let record = publish(root.path(), &receipt);
    let report = build_report(&spec("A01")?, &[record], Some(CANDIDATE));
    assert!(!report.requirements[0].is_met());
    assert!(
        report.requirements[0]
            .unmet_reasons
            .contains(&UnmetReason::ReceiptUnproven)
    );
    Ok(())
}

#[test]
fn required_model_scope_is_not_borrowed_from_native_or_stale_evidence() -> Result<(), IndexError> {
    let root = tempfile::tempdir().unwrap();
    let mut receipt = fixture(root.path(), "A06");
    let record = publish(root.path(), &receipt);
    let index = spec("A06")?;
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
    Ok(())
}

/// Write a report (or a mutated copy of one) and return its path and digest.
fn write_report(root: &Path, name: &str, report: &Value) -> (PathBuf, String) {
    let bytes = serde_json::to_vec(report).unwrap();
    let path = root.join(name);
    fs::write(&path, &bytes).unwrap();
    (path, digest(&bytes))
}

fn rejection(root: &Path, name: &str, report: &Value, candidate: Option<&str>) -> String {
    let (path, sha256) = write_report(root, name, report);
    let input = LongRunInput {
        path: &path,
        sha256: &sha256,
    };
    let summary = summarize([&input], candidate);
    assert!(summary.admitted.is_empty(), "{name} was admitted");
    assert_eq!(summary.duration_status(), "unobserved");
    summary.rejections[&path.to_string_lossy().into_owned()].clone()
}

#[test]
fn acceptance_matrix_needs_one_admitted_receipt_for_every_declared_scope() -> Result<(), IndexError> {
    let root = tempfile::tempdir().unwrap();
    let receipt = fixture(root.path(), "K01");
    let native_only = publish(root.path(), &receipt);
    let index = spec("K01")?;

    let partial = build_report(&index, std::slice::from_ref(&native_only), Some(CANDIDATE));
    let case = &partial.cases[0];
    assert_eq!(case.case_id, "K01");
    assert_eq!(case.required_scopes, vec![Scope::Model, Scope::Native]);
    assert_eq!(case.proven_scopes, vec![Scope::Native]);
    assert_eq!(case.status(), "partial");
    assert!(case.proven_by.is_empty());
    assert!(!partial.requirements[0].is_met());
    assert_eq!(
        partial.to_json()["case_status_counts"]["partial"].as_u64(),
        Some(1)
    );

    // The same case covered by one receipt that carries both scopes is
    // proven; two separate receipts never add up to that.
    let mut complete = receipt;
    complete["cases"][0]["observations"]["model"] = json!({
        "status": "passed", "checks": {"model_condition": true},
        "output": bound_file(root.path(), "model.txt", b"synthetic model observation")
    });
    let proven = publish(root.path(), &complete);
    let report = build_report(&index, &[proven], Some(CANDIDATE));
    assert_eq!(report.cases[0].status(), "proven");
    assert_eq!(report.cases[0].proven_by, ["fixture"]);
    assert_eq!(
        report.to_json()["case_status_counts"]["proven"].as_u64(),
        Some(1)
    );
    assert!(report.requirements[0].is_met());
    Ok(())
}

#[test]
fn a_correction_record_withdraws_evidence_without_deleting_its_bytes() -> Result<(), IndexError> {
    let root = tempfile::tempdir().unwrap();
    let receipt = fixture(root.path(), "A01");
    let record = publish(root.path(), &receipt);
    let index = spec("A01")?;
    assert!(
        build_report(&index, std::slice::from_ref(&record), Some(CANDIDATE)).requirements[0]
            .is_met()
    );

    let correction = json!({
        "evidence_id": "fixture-correction",
        "withdraws": "fixture",
        "withdrawn": "the receipt observed a different scenario than it claims",
    });
    let path = root.path().join("correction.json");
    fs::write(&path, serde_json::to_vec(&correction).unwrap()).unwrap();
    let correction = EvidenceRecord::from_json(&correction, path.to_str().unwrap()).unwrap();
    let report = build_report(&index, &[record.clone(), correction], Some(CANDIDATE));

    assert!(!report.requirements[0].is_met());
    assert!(
        report.requirements[0]
            .unmet_reasons
            .contains(&UnmetReason::Withdrawn)
    );
    assert_eq!(report.cases[0].status(), "unproven");
    assert_eq!(
        report.withdrawals["fixture"],
        "the receipt observed a different scenario than it claims"
    );
    assert!(report.to_json()["withdrawals"]["fixture"].is_string());
    // The correction states `withdrawn` as the reason for withdrawing the
    // original; it is not itself withdrawn, and reporting it as both would
    // hide which record is the declaration.
    assert!(
        !report.withdrawals.contains_key("fixture-correction"),
        "{:?}",
        report.withdrawals
    );
    assert!(
        report.to_json()["withdrawals"]["fixture-correction"].is_null(),
        "{}",
        report.to_json()["withdrawals"]
    );
    // Retained bytes are still there: the correction is a declaration.
    assert!(root.path().join("receipt.json").is_file());

    // A correction that names a record nobody retained withdraws nothing; the
    // mismatch is reported instead of being taken as a withdrawal.
    let typo = json!({
        "evidence_id": "fixture-typo",
        "withdraws": "no-such-evidence",
    });
    let typo_path = root.path().join("typo.json");
    fs::write(&typo_path, serde_json::to_vec(&typo).unwrap()).unwrap();
    let typo = EvidenceRecord::from_json(&typo, typo_path.to_str().unwrap()).unwrap();
    let report = build_report(&index, &[record, typo], Some(CANDIDATE));
    assert!(report.requirements[0].is_met());
    assert_eq!(report.dangling_withdrawals, ["no-such-evidence"]);
    Ok(())
}

#[test]
fn long_run_report_must_bind_this_candidate_and_its_measured_binary() {
    let root = tempfile::tempdir().unwrap();
    let report = sealed_run_report();
    let (path, sha256) = write_report(root.path(), "sealed.json", &report);
    let input = LongRunInput {
        path: &path,
        sha256: &sha256,
    };

    let admitted = summarize([&input], Some(CANDIDATE));
    assert_eq!(admitted.duration_status(), "observed");
    assert!(admitted.rejections.is_empty());
    let admission = &admitted.admitted[0];
    assert_eq!(admission.run_id, "fixture-run");
    assert_eq!(admission.candidate_commit, CANDIDATE);
    assert_eq!(admission.profile, "release-native");
    assert_eq!(admission.binary_sha256, BINARY);
    assert_eq!(admission.stop_reason, "completed");
    assert_eq!(admission.stop_sample_seq, 2);
    assert_eq!(admission.observed_windows(), ["7d", "30d"]);
    assert_eq!(admission.checkpoints[0].checkpoint_id, "review-1");
    assert_eq!(
        admitted.to_json()["admitted_reports"][0]["profile"].as_str(),
        Some("release-native")
    );

    // The caller-supplied digest is the only proof of the bytes read.
    let zeros = "0".repeat(64);
    let wrong_digest = LongRunInput {
        path: &path,
        sha256: &zeros,
    };
    let summary = summarize([&wrong_digest], Some(CANDIDATE));
    let reason = summary.rejections[&path.to_string_lossy().into_owned()].clone();
    assert!(reason.contains("SHA-256 mismatch"), "{reason}");
    // A rejected report still keeps its declared windows in the denominator.
    assert_eq!(summary.unobserved_windows(), ["30d", "7d"]);

    let other = rejection(
        root.path(),
        "other-candidate.json",
        &report,
        Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
    );
    assert!(other.contains("another candidate"), "{other}");

    let missing = rejection(root.path(), "no-candidate.json", &report, None);
    assert!(missing.contains("candidate identity is missing"), "{missing}");

    let mut unmeasured = report.clone();
    unmeasured["candidate"]["binary_measured"]["sha256"] = Value::String("d".repeat(64));
    let reason = rejection(root.path(), "unmeasured.json", &unmeasured, Some(CANDIDATE));
    assert!(
        reason.contains("measured candidate binary does not match"),
        "{reason}"
    );

    let mut old_schema = report.clone();
    old_schema["schema"] = json!(3);
    let reason = rejection(root.path(), "schema-3.json", &old_schema, Some(CANDIDATE));
    assert!(reason.contains("unsupported long-run report schema 3"), "{reason}");
}

#[test]
fn sealed_stat_only_reports_cannot_certify_the_executed_binary() {
    let root = tempfile::tempdir().unwrap();
    let mut report = sealed_run_report();
    report["schema"] = json!(4);
    rejection(root.path(), "stat-only.json", &report, Some(CANDIDATE));
}

#[test]
fn missing_executable_binding_remains_unproven() {
    let root = tempfile::tempdir().unwrap();
    let mut report = sealed_run_report();
    report["integrity"].as_object_mut().unwrap().remove("candidate_binary_binding");
    rejection(root.path(), "missing-binding.json", &report, Some(CANDIDATE));
}

#[test]
fn executable_verification_must_cover_every_live_sample() {
    let root = tempfile::tempdir().unwrap();
    let mut report = sealed_run_report();
    report["integrity"]["candidate_binary_verified_sample_seqs"] = json!([1]);
    rejection(root.path(), "partial-binding.json", &report, Some(CANDIDATE));
}

#[test]
fn repeated_executable_samples_cannot_inflate_coverage() {
    let root = tempfile::tempdir().unwrap();
    let mut report = sealed_run_report();
    report["integrity"]["candidate_binary_verified_sample_seqs"] = json!([1, 1]);
    rejection(root.path(), "repeated-binding.json", &report, Some(CANDIDATE));
}

#[test]
fn long_run_duration_requires_a_completed_sealed_run_of_the_declared_window() {
    let root = tempfile::tempdir().unwrap();
    let report = sealed_run_report();

    let mut unsealed = report.clone();
    unsealed["lifecycle"]["sealed"] = json!(false);
    let reason = rejection(root.path(), "unsealed.json", &unsealed, Some(CANDIDATE));
    assert!(reason.contains("was not sealed"), "{reason}");

    let mut aborted = report.clone();
    aborted["lifecycle"]["stop_reason"] = json!("aborted");
    let reason = rejection(root.path(), "aborted.json", &aborted, Some(CANDIDATE));
    assert!(reason.contains("aborted"), "{reason}");

    let mut early_stop = report.clone();
    early_stop["lifecycle"]["stop_epoch"] = json!(1000.0);
    let reason = rejection(root.path(), "early-stop.json", &early_stop, Some(CANDIDATE));
    assert!(
        reason.contains("stopped before its last recorded sample"),
        "{reason}"
    );

    let mut short = report.clone();
    short["declared_windows"][1]["qualifying_span_seconds"] = json!(604800);
    let reason = rejection(root.path(), "short.json", &short, Some(CANDIDATE));
    assert!(reason.contains("30d"), "{reason}");

    let mut unknown = report.clone();
    unknown["verdict"] = json!("probably_fine");
    let reason = rejection(root.path(), "unknown-verdict.json", &unknown, Some(CANDIDATE));
    assert!(reason.contains("unknown report verdict"), "{reason}");

    // A seal names a sample the report actually recorded; claiming a later
    // one is not terminal completion of these bytes.
    let mut past = report.clone();
    past["lifecycle"]["stop_sample_seq"] = json!(9);
    let reason = rejection(root.path(), "past-recorded.json", &past, Some(CANDIDATE));
    assert!(
        reason.contains("past its 2 recorded samples"),
        "{reason}"
    );

    // A run that declared 7d+30d and was stopped after only the 7-day window
    // qualifies is real 7-day evidence, and its 30-day window stays owed.
    let mut partial = report.clone();
    partial["declared_windows"][0]["status"] = json!("unobserved");
    partial["declared_windows"][0]["qualifying_span_seconds"] = json!(600);
    partial["verdict"] = json!("incomplete_observation");
    let (path, sha256) = write_report(root.path(), "partial.json", &partial);
    let input = LongRunInput {
        path: &path,
        sha256: &sha256,
    };
    let summary = summarize([&input], Some(CANDIDATE));
    assert!(summary.rejections.is_empty());
    assert_eq!(summary.admitted[0].windows.len(), 2);
    assert_eq!(summary.admitted[0].observed_windows(), ["30d"]);
    assert_eq!(summary.unobserved_windows(), ["7d"]);
    assert_eq!(summary.duration_status(), "unobserved");

    // Naming a window "7d" while requiring a minute is a different
    // obligation under the SPEC's name, so it is refused rather than read as
    // short 7-day evidence.
    let mut mislabeled = report.clone();
    mislabeled["declared_windows"][0]["required_seconds"] = json!(60);
    mislabeled["declared_windows"][0]["qualifying_span_seconds"] = json!(60.0);
    mislabeled["declared_windows"][0]["status"] = json!("observed");
    let reason = rejection(root.path(), "mislabeled.json", &mislabeled, Some(CANDIDATE));
    assert!(reason.contains("less than the 604800"), "{reason}");

    let mut synthetic = report;
    synthetic["evidence_class"] = json!("synthetic_self_check");
    let reason = rejection(root.path(), "synthetic.json", &synthetic, Some(CANDIDATE));
    assert!(reason.contains("cannot discharge"), "{reason}");
}

#[test]
fn obligation_report_states_duration_only_from_an_admitted_sealed_report() -> Result<(), IndexError> {
    let root = tempfile::tempdir().unwrap();
    let evidence = root.path().join("evidence");
    fs::create_dir(&evidence).unwrap();
    let (path, sha256) = write_report(root.path(), "sealed.json", &sealed_run_report());
    let input = LongRunInput {
        path: &path,
        sha256: &sha256,
    };
    let spec_text =
        "<a id=\"fixture\"></a>\n| R001 | fixture | §23 | U01;B01 |\n| B01 fixture | R001 | contract | A01 |\n| A01 | scenario | condition |\n";
    let index = SpecIndex::parse("fixture.md", spec_text)?;

    let observed = analyze(ReportInputs {
        spec_index: index,
        evidence_dir: &evidence,
        candidate_commit: Some(CANDIDATE),
        long_run: std::slice::from_ref(&input),
    });
    let json = observed.to_json();
    assert_eq!(
        json["long_run_observation"]["duration_status"].as_str(),
        Some("observed")
    );
    assert_eq!(
        json["long_run_observation"]["admitted_reports"][0]["candidate_commit"].as_str(),
        Some(CANDIDATE)
    );
    assert_eq!(json["schema_version"].as_u64(), Some(3));
    assert!(observed.summary().contains("long-run duration: observed"));

    // No report is the normal state: the obligation report must not infer
    // duration from the existence of the tooling.
    let unobserved = analyze(ReportInputs {
        spec_index: observed.index,
        evidence_dir: &evidence,
        candidate_commit: Some(CANDIDATE),
        long_run: &[],
    });
    let json = unobserved.to_json();
    assert_eq!(
        json["long_run_observation"]["duration_status"].as_str(),
        Some("unobserved")
    );
    assert!(json["long_run_observation"]["windows"]
        .as_object()
        .is_some_and(|windows| windows.is_empty()));
    assert!(unobserved.summary().contains("long-run duration: unobserved"));

    // A sealed report whose only window is a one-minute demo is admitted as
    // that observation, but it is not the SPEC's 7/30-day duration.
    let mut demo = sealed_run_report();
    demo["declared_windows"] = json!([{
        "window": "demo",
        "required_seconds": 60,
        "qualifying_span_seconds": 60,
        "status": "observed",
    }]);
    let (demo_path, demo_sha) = write_report(root.path(), "demo.json", &demo);
    let demo_input = LongRunInput {
        path: &demo_path,
        sha256: &demo_sha,
    };
    let demo = analyze(ReportInputs {
        spec_index: unobserved.index,
        evidence_dir: &evidence,
        candidate_commit: Some(CANDIDATE),
        long_run: std::slice::from_ref(&demo_input),
    });
    let json = demo.to_json();
    assert_eq!(
        json["long_run_observation"]["duration_status"].as_str(),
        Some("unobserved")
    );
    assert_eq!(
        json["long_run_observation"]["unobserved_windows"]
            .as_array()
            .unwrap()
            .iter()
            .map(Value::as_str)
            .collect::<Vec<_>>(),
        [Some("30d"), Some("7d")]
    );
    assert_eq!(
        json["long_run_observation"]["obligation_windows"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    Ok(())
}

/// Two admitted reports of the same commit but different artifacts never add
/// up to one duration observation.
#[test]
fn duration_windows_only_add_up_for_one_binary_and_profile() -> Result<(), IndexError> {
    let root = tempfile::tempdir().unwrap();
    let mut seven = sealed_run_report();
    seven["declared_windows"][0]["status"] = json!("observed");
    seven["declared_windows"][1]["status"] = json!("unobserved");
    seven["declared_windows"][1]["qualifying_span_seconds"] = json!(0.0);
    seven["verdict"] = json!("incomplete_observation");

    let mut thirty = sealed_run_report();
    thirty["declared_windows"][0]["status"] = json!("unobserved");
    thirty["declared_windows"][0]["qualifying_span_seconds"] = json!(0.0);
    thirty["verdict"] = json!("incomplete_observation");

    let (seven_path, seven_sha) = write_report(root.path(), "seven.json", &seven);
    let (thirty_path, thirty_sha) = write_report(root.path(), "thirty.json", &thirty);
    let seven_input = LongRunInput {
        path: &seven_path,
        sha256: &seven_sha,
    };
    let thirty_input = LongRunInput {
        path: &thirty_path,
        sha256: &thirty_sha,
    };

    // One artifact, one profile: the two windows really are one observation.
    let summary = summarize([&seven_input, &thirty_input], Some(CANDIDATE));
    assert!(summary.rejections.is_empty(), "{:?}", summary.rejections);
    assert_eq!(summary.admitted.len(), 2);
    assert!(summary.identity_conflicts().is_empty());
    assert_eq!(summary.unobserved_windows(), Vec::<String>::new());
    assert_eq!(summary.duration_status(), "observed");

    // The same windows observed on a different binary and profile do not
    // combine: nobody ran a 30-day window of this artifact.
    let other_binary = "e".repeat(64);
    let mut other = thirty.clone();
    other["candidate"]["binary_sha256"] = Value::String(other_binary.clone());
    other["candidate"]["binary_measured"]["sha256"] = Value::String(other_binary.clone());
    other["candidate"]["profile"] = json!("debug-native");
    let (other_path, other_sha) = write_report(root.path(), "other-artifact.json", &other);
    let other_input = LongRunInput {
        path: &other_path,
        sha256: &other_sha,
    };
    let summary = summarize([&seven_input, &other_input], Some(CANDIDATE));
    assert!(summary.rejections.is_empty(), "{:?}", summary.rejections);
    assert_eq!(summary.admitted.len(), 2);
    assert_eq!(summary.duration_status(), "unobserved");
    let conflicts = summary.identity_conflicts();
    assert_eq!(conflicts.len(), 2, "{conflicts:?}");
    assert!(conflicts[0].contains(&other_binary), "{conflicts:?}");
    assert!(conflicts[0].contains(BINARY), "{conflicts:?}");
    assert!(conflicts[1].contains("debug-native"), "{conflicts:?}");
    assert!(conflicts[1].contains("release-native"), "{conflicts:?}");
    let json = summary.to_json();
    assert_eq!(json["duration_status"].as_str(), Some("unobserved"));
    assert_eq!(json["identity_conflicts"].as_array().unwrap().len(), 2);
    assert_eq!(json["candidate_binaries"].as_array().unwrap().len(), 2);
    assert_eq!(json["profiles"].as_array().unwrap().len(), 2);

    // And the obligation report says the same thing, with the conflict shown.
    let evidence = root.path().join("evidence-conflict");
    fs::create_dir(&evidence).unwrap();
    let spec_text =
        "<a id=\"fixture\"></a>\n| R001 | fixture | §23 | B01 |\n| B01 fixture | R001 | contract | A01 |\n| A01 | scenario | condition |\n";
    let index = SpecIndex::parse("fixture.md", spec_text)?;
    let analysis = analyze(ReportInputs {
        spec_index: index,
        evidence_dir: &evidence,
        candidate_commit: Some(CANDIDATE),
        long_run: &[seven_input, other_input],
    });
    let json = analysis.to_json();
    assert_eq!(
        json["long_run_observation"]["duration_status"].as_str(),
        Some("unobserved")
    );
    assert_eq!(
        json["long_run_observation"]["identity_conflicts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        analysis
            .summary()
            .contains("long-run identity conflict: admitted reports measure"),
        "{}",
        analysis.summary()
    );
    Ok(())
}
