//! Consumer regressions for the withdrawal producer.
//!
//! These run the real `shenko-evidence` binary over real files: the correction
//! is produced, the original bytes are compared before and after, the report
//! reopens the directory, and every refusal path is observed. A produced
//! correction is provenance for one claim; it is not product acceptance.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use shenko_evidence::{CorrectionRequest, correction_id, write_correction};

const CANDIDATE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

const SPEC_TEXT: &str = "<a id=\"fixture\"></a>\n| R001 | fixture | §23 | B01 |\n| B01 fixture | R001 | contract | A01 |\n| A01 | scenario | condition |\n";

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn write_record(directory: &Path, evidence_id: &str) -> (std::path::PathBuf, Vec<u8>) {
    let record = json!({
        "schema_version": 1,
        "evidence_id": evidence_id,
        "candidate_commit": CANDIDATE,
        "status": "passed",
        "requirements": ["R001"],
        "full_cases_passed": ["A01"],
    });
    let mut bytes = serde_json::to_vec_pretty(&record).unwrap();
    bytes.push(b'\n');
    let path = directory.join(format!("{evidence_id}.json"));
    fs::write(&path, &bytes).unwrap();
    (path, bytes)
}

fn withdraw_command(evidence: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_shenko-evidence"));
    command.arg("--evidence-dir").arg(evidence).arg("--withdraw");
    command
}

fn report_command(spec: &Path, evidence: &Path, out: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_shenko-evidence"));
    command.args([
        "--spec",
        spec.to_str().unwrap(),
        "--evidence-dir",
        evidence.to_str().unwrap(),
        "--candidate-commit",
        CANDIDATE,
        "--json",
        out.to_str().unwrap(),
    ]);
    command
}

#[test]
fn cli_writes_a_digest_bound_correction_and_the_report_reopens_it() {
    let root = tempfile::tempdir().unwrap();
    let evidence = root.path().join("evidence");
    fs::create_dir(&evidence).unwrap();
    let (original_path, original_bytes) = write_record(&evidence, "fixture-evidence");
    let original_digest = digest(&original_bytes);
    let reason = "the receipt observed a different scenario than it claims";
    let expected_id = correction_id("fixture-evidence");

    let outcome_path = root.path().join("outcome.json");
    let run = withdraw_command(&evidence)
        .args([
            "fixture-evidence",
            "--reason",
            reason,
            "--source-sha256",
            original_digest.as_str(),
            "--json",
        ])
        .arg(&outcome_path)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    let correction_path = evidence.join(format!("{expected_id}.json"));
    assert!(correction_path.is_file());
    assert_eq!(
        fs::read(&original_path).unwrap(),
        original_bytes,
        "the original record must keep its exact bytes"
    );

    let correction: Value =
        serde_json::from_slice(&fs::read(&correction_path).unwrap()).unwrap();
    assert_eq!(correction["evidence_id"].as_str(), Some(expected_id.as_str()));
    assert_eq!(correction["withdraws"].as_str(), Some("fixture-evidence"));
    assert_eq!(
        correction["withdraws_sha256"].as_str(),
        Some(original_digest.as_str())
    );
    assert_eq!(correction["withdrawn"].as_str(), Some(reason));
    for key in [
        "requirements",
        "full_cases_passed",
        "cases_reference_only",
        "status",
        "native_receipt",
    ] {
        assert!(
            correction.get(key).is_none(),
            "a correction must not carry product claims: {key}"
        );
    }

    let outcome: Value = serde_json::from_slice(&fs::read(&outcome_path).unwrap()).unwrap();
    assert_eq!(outcome["withdraws"].as_str(), Some("fixture-evidence"));
    assert_eq!(
        outcome["withdraws_sha256"].as_str(),
        Some(original_digest.as_str())
    );
    assert!(outcome.get("evidence_id").is_none());

    // Reopen through the real report command, which reads the same directory.
    let spec = root.path().join("SPEC.md");
    fs::write(&spec, SPEC_TEXT).unwrap();
    let report_path = root.path().join("report.json");
    let run = report_command(&spec, &evidence, &report_path)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let report: Value = serde_json::from_slice(&fs::read(&report_path).unwrap()).unwrap();
    assert_eq!(
        report["withdrawals"]["fixture-evidence"].as_str(),
        Some(reason)
    );
    assert!(
        report["dangling_withdrawals"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        report["acceptance_case_matrix"][0]["status"].as_str(),
        Some("unproven")
    );
    assert_eq!(report["unmet_requirement_count"].as_u64(), Some(1));
    assert_eq!(
        report["evidence_inventory"]["complete"].as_bool(),
        Some(true)
    );

    let entries = report["evidence_inventory"]["entries"]
        .as_array()
        .unwrap();
    let original = entries
        .iter()
        .find(|entry| entry["evidence_id"].as_str() == Some("fixture-evidence"))
        .unwrap();
    assert!(original["withdrawn"].is_null());
    assert!(original["withdraws"].is_null());
    let correction_entry = entries
        .iter()
        .find(|entry| entry["evidence_id"].as_str() == Some(expected_id.as_str()))
        .unwrap();
    assert_eq!(
        correction_entry["withdraws"].as_str(),
        Some("fixture-evidence")
    );
    assert_eq!(
        correction_entry["withdraws_sha256"].as_str(),
        Some(original_digest.as_str())
    );
    let correction_source = correction_entry["source_path"].as_str().unwrap();
    let rejection = report["evidence_rejections"][correction_source]
        .as_str()
        .unwrap();
    assert!(rejection.contains("withdrawal declaration"), "{rejection}");

    assert_eq!(fs::read(&original_path).unwrap(), original_bytes);
}

#[test]
fn cli_refuses_missing_mismatched_duplicate_and_overwriting_references() {
    let root = tempfile::tempdir().unwrap();
    let evidence = root.path().join("evidence");
    fs::create_dir(&evidence).unwrap();
    let (original_path, original_bytes) = write_record(&evidence, "fixture-evidence");
    let original_digest = digest(&original_bytes);
    let expected_path = evidence.join(format!("{}.json", correction_id("fixture-evidence")));

    // The reviewed digest is part of the binding: different bytes are refused.
    let wrong = "f".repeat(64);
    let run = withdraw_command(&evidence)
        .args([
            "fixture-evidence",
            "--reason",
            "wrong bytes",
            "--source-sha256",
            wrong.as_str(),
        ])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&run.stderr).contains("source digest mismatch"),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(!expected_path.exists());

    // A missing id is a missing reference, not a new fact.
    let run = withdraw_command(&evidence)
        .args([
            "absent-evidence",
            "--reason",
            "nothing to withdraw",
            "--source-sha256",
            original_digest.as_str(),
        ])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&run.stderr).contains("no retained evidence record"),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(!evidence.join("absent-evidence-withdrawal.json").exists());

    // The reason is required; a blank string is not a reason.
    let run = withdraw_command(&evidence)
        .args([
            "fixture-evidence",
            "--reason",
            "   ",
            "--source-sha256",
            original_digest.as_str(),
        ])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&run.stderr).contains("explicit reason"),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    // A supersession must name a record that actually exists.
    let run = withdraw_command(&evidence)
        .args([
            "fixture-evidence",
            "--reason",
            "replaced by nothing",
            "--source-sha256",
            original_digest.as_str(),
            "--superseded-by",
            "no-such-replacement",
        ])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&run.stderr)
            .contains("superseded_by names no retained evidence record"),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    // Duplicate ids are ambiguous, so no exact record can be referenced.
    fs::write(
        evidence.join("duplicate.json"),
        serde_json::to_vec(&json!({
            "evidence_id": "fixture-evidence",
            "candidate_commit": CANDIDATE,
            "status": "passed",
        }))
        .unwrap(),
    )
    .unwrap();
    let run = withdraw_command(&evidence)
        .args([
            "fixture-evidence",
            "--reason",
            "ambiguous target",
            "--source-sha256",
            original_digest.as_str(),
        ])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&run.stderr).contains("names 2 retained records"),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    fs::remove_file(evidence.join("duplicate.json")).unwrap();

    // An existing path is never overwritten, even when it is not evidence.
    let existing = b"{\"note\":\"not evidence\"}\n";
    fs::write(&expected_path, existing).unwrap();
    let run = withdraw_command(&evidence)
        .args([
            "fixture-evidence",
            "--reason",
            "first correction",
            "--source-sha256",
            original_digest.as_str(),
        ])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&run.stderr).contains("refusing to overwrite"),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(fs::read(&expected_path).unwrap(), existing);

    // A real correction succeeds once, and a second one is refused.
    fs::remove_file(&expected_path).unwrap();
    let run = withdraw_command(&evidence)
        .args([
            "fixture-evidence",
            "--reason",
            "first correction",
            "--source-sha256",
            original_digest.as_str(),
        ])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let run = withdraw_command(&evidence)
        .args([
            "fixture-evidence",
            "--reason",
            "second correction",
            "--source-sha256",
            original_digest.as_str(),
        ])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&run.stderr).contains("already withdrawn"),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    // The declaration that was just written cannot itself be corrected.
    let correction_bytes = fs::read(&expected_path).unwrap();
    let run = withdraw_command(&evidence)
        .args([
            correction_id("fixture-evidence").as_str(),
            "--reason",
            "second order correction",
            "--source-sha256",
            digest(&correction_bytes).as_str(),
        ])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&run.stderr).contains("withdrawal declaration"),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    assert_eq!(fs::read(&original_path).unwrap(), original_bytes);
}

#[test]
fn api_names_a_real_replacement_and_refuses_a_stale_digest() {
    let root = tempfile::tempdir().unwrap();
    let evidence = root.path().join("evidence");
    fs::create_dir(&evidence).unwrap();
    let (original_path, original_bytes) = write_record(&evidence, "fixture-evidence");
    let (replacement_path, replacement_bytes) = write_record(&evidence, "replacement-evidence");

    let outcome = write_correction(&CorrectionRequest {
        evidence_dir: &evidence,
        evidence_id: "fixture-evidence",
        reason: "superseded by a corrected rerun",
        source_sha256: &digest(&original_bytes),
        superseded_by: Some("replacement-evidence"),
    })
    .unwrap();
    assert_eq!(outcome.withdraws, "fixture-evidence");
    assert_eq!(
        outcome.withdraws_sha256,
        digest(&original_bytes)
    );
    let written: Value =
        serde_json::from_slice(&fs::read(&outcome.correction_path).unwrap()).unwrap();
    assert_eq!(
        written["superseded_by"].as_str(),
        Some("replacement-evidence")
    );
    assert_eq!(fs::read(&original_path).unwrap(), original_bytes);
    assert_eq!(fs::read(&replacement_path).unwrap(), replacement_bytes);

    // A digest that no longer matches is a refusal, not a new correction.
    let error = write_correction(&CorrectionRequest {
        evidence_dir: &evidence,
        evidence_id: "replacement-evidence",
        reason: "stale digest",
        source_sha256: &digest(b"different bytes"),
        superseded_by: None,
    })
    .unwrap_err();
    assert!(error.contains("source digest mismatch"), "{error}");
    assert!(
        !evidence
            .join(format!(
                "{}.json",
                correction_id("replacement-evidence")
            ))
            .exists()
    );

    // A correction declaration is not evidence and cannot be corrected.
    let correction_name = correction_id("fixture-evidence");
    let correction_bytes = fs::read(&outcome.correction_path).unwrap();
    let error = write_correction(&CorrectionRequest {
        evidence_dir: &evidence,
        evidence_id: correction_name.as_str(),
        reason: "second order correction",
        source_sha256: &digest(&correction_bytes),
        superseded_by: None,
    })
    .unwrap_err();
    assert!(error.contains("withdrawal declaration"), "{error}");
}

#[test]
fn report_stops_honoring_a_correction_whose_bound_bytes_changed() {
    let root = tempfile::tempdir().unwrap();
    let evidence = root.path().join("evidence");
    fs::create_dir(&evidence).unwrap();
    let (original_path, original_bytes) = write_record(&evidence, "fixture-evidence");
    let reason = "the observed output was edited after review";
    let run = withdraw_command(&evidence)
        .args([
            "fixture-evidence",
            "--reason",
            reason,
            "--source-sha256",
            digest(&original_bytes).as_str(),
        ])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    // Change the original bytes after the correction was written. The
    // correction still names the same id, but no longer the same facts.
    let mut tampered: Value =
        serde_json::from_slice(&fs::read(&original_path).unwrap()).unwrap();
    tampered["note"] = json!("edited after review");
    let tampered_bytes = serde_json::to_vec_pretty(&tampered).unwrap();
    fs::write(&original_path, &tampered_bytes).unwrap();

    let spec = root.path().join("SPEC.md");
    fs::write(&spec, SPEC_TEXT).unwrap();
    let report_path = root.path().join("report.json");
    let run = report_command(&spec, &evidence, &report_path)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let report: Value = serde_json::from_slice(&fs::read(&report_path).unwrap()).unwrap();

    // The withdrawal is not applied in the correction's favour, and the
    // original is admitted (or rejected) on its own current bytes.
    assert!(report["withdrawals"]["fixture-evidence"].is_null());
    assert!(
        report["dangling_withdrawals"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let entries = report["evidence_inventory"]["entries"]
        .as_array()
        .unwrap();
    let correction_entry = entries
        .iter()
        .find(|entry| {
            entry["evidence_id"].as_str()
                == Some(correction_id("fixture-evidence").as_str())
        })
        .unwrap();
    let correction_source = correction_entry["source_path"].as_str().unwrap();
    let rejection = report["evidence_rejections"][correction_source]
        .as_str()
        .unwrap();
    assert!(
        rejection.contains("does not match the retained bytes"),
        "{rejection}"
    );

    let reasons: Vec<&str> = report["requirements"][0]["unmet_reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert!(!reasons.contains(&"withdrawn"), "{reasons:?}");
    assert!(reasons.contains(&"receipt_unproven"), "{reasons:?}");

    // Reporting never deletes or rewrites either file.
    assert_eq!(fs::read(&original_path).unwrap(), tampered_bytes);
}
