//! The `shenko-evidence` executable is the report surface's only entry point;
//! these are its consumer regressions. They run the real binary over real
//! SPEC/evidence files and read the report it wrote — no assertions on source
//! text.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
use sha2::{Digest, Sha256};

mod support;

use support::{BINARY, CANDIDATE, sealed_run_report};

const SPEC_TEXT: &str = "<a id=\"fixture\"></a>\n| R001 | fixture | §23 | B01 |\n| B01 fixture | R001 | contract | A01 |\n| A01 | scenario | condition |\n";

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

fn read_report(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn cli_writes_the_obligation_report_and_binds_a_sealed_long_run_report() {
    let root = tempfile::tempdir().unwrap();
    let spec = root.path().join("SPEC.md");
    fs::write(&spec, SPEC_TEXT).unwrap();
    let evidence = root.path().join("evidence");
    fs::create_dir(&evidence).unwrap();

    // Without a retained long-run report the duration obligation is honestly
    // unobserved: the tooling existing never becomes a duration claim.
    let without_duration = root.path().join("report.json");
    let run = report_command(&spec, &evidence, &without_duration)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        String::from_utf8_lossy(&run.stdout).contains("long-run duration: unobserved"),
        "{}",
        String::from_utf8_lossy(&run.stdout)
    );
    let report = read_report(&without_duration);
    assert_eq!(
        report["kind"].as_str(),
        Some("shenko_spec_obligation_report")
    );
    assert_eq!(report["candidate_commit"].as_str(), Some(CANDIDATE));
    assert_eq!(report["denominator"]["acceptance_cases"].as_u64(), Some(1));
    assert_eq!(report["unmet_requirement_count"].as_u64(), Some(1));
    let cases = report["acceptance_case_matrix"].as_array().unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0]["case_id"].as_str(), Some("A01"));
    assert_eq!(cases[0]["status"].as_str(), Some("unproven"));
    assert_eq!(
        report["long_run_observation"]["duration_status"].as_str(),
        Some("unobserved")
    );

    // A sealed report of this candidate, bound by its digest, is admitted.
    let sampler = root.path().join("sampler.json");
    let sampler_bytes = serde_json::to_vec(&sealed_run_report()).unwrap();
    fs::write(&sampler, &sampler_bytes).unwrap();
    let binding = format!(
        "{}={:x}",
        sampler.to_str().unwrap(),
        Sha256::digest(&sampler_bytes)
    );
    let with_duration = root.path().join("report-with-duration.json");
    let run = report_command(&spec, &evidence, &with_duration)
        .arg("--long-run")
        .arg(&binding)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let report = read_report(&with_duration);
    assert_eq!(
        report["long_run_observation"]["duration_status"].as_str(),
        Some("observed")
    );
    let admitted = &report["long_run_observation"]["admitted_reports"][0];
    assert_eq!(admitted["run_id"].as_str(), Some("fixture-run"));
    assert_eq!(admitted["stop_reason"].as_str(), Some("completed"));
    assert_eq!(admitted["binary_sha256"].as_str(), Some(BINARY));
    assert_eq!(admitted["checkpoint_count"].as_u64(), Some(1));
    assert_eq!(admitted["windows"][1]["name"].as_str(), Some("30d"));
}

#[test]
fn cli_refuses_missing_arguments_and_unreadable_specs() {
    let run = Command::new(env!("CARGO_BIN_EXE_shenko-evidence"))
        .arg("--spec")
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&run.stderr).contains("needs a value"),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    let root = tempfile::tempdir().unwrap();
    let evidence = root.path().join("evidence");
    fs::create_dir(&evidence).unwrap();
    let out = root.path().join("report.json");
    let run = report_command(&root.path().join("absent.md"), &evidence, &out)
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&run.stderr).contains("cannot read"),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(!out.exists());
}

#[test]
fn cli_keeps_appended_obligations_and_requires_document_local_owners()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir().unwrap();
    let docs = root.path().join("docs");
    fs::create_dir(&docs)?;
    let evidence = root.path().join("evidence");
    fs::create_dir(&evidence).unwrap();
    let current = docs.join("current.md");
    fs::write(&current, format!("{SPEC_TEXT}{}", r#"
````markdown
```example
<a id="fixture"></a>
| R901 | fenced example | example | B91 |
| B91 example | R901 | example | A91 |
| A91 | example | example |
````

~~~markdown
| R902 | tilde example | example | B92 |
| B92 example | R902 | example | A92 |
| A92 | example | example |
~~~

<a id="appended"></a>
| R002 | appended obligation | current | B02 |
| B02 appended | R002 | contract | A02 |
| A02 | appended scenario | appended condition |

```` unclosed fence stays in this document
"#))?;
    let next = docs.join("next.md");
    // Reusing a name in a different document does not duplicate its owner.
    fs::write(&next, "<a id=\"fixture\"></a>\n| R003 | new document | current | B03 |\n| B03 new | R003 | contract | A01/A03 |\n| A03 | new scenario | new condition |\n")?;
    let inventory = docs.join("contracts.json");
    fs::write(&inventory, serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "documents": ["docs/current.md", "docs/next.md"],
    }))?)?;
    let out = root.path().join("report.json");
    let run = report_command(&inventory, &evidence, &out).output()?;
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    let report = read_report(&out);
    for collection in ["requirements", "behaviors", "acceptance_cases"] {
        assert_eq!(report["spec_index_counts"][collection].as_u64(), Some(3));
        assert_eq!(report["spec_index"][collection][0]["source_path"], "docs/current.md");
        assert_eq!(report["spec_index"][collection][0]["source_anchor"], "fixture");
        assert_eq!(report["spec_index"][collection][1]["source_anchor"], "appended");
        assert_eq!(report["spec_index"][collection][2]["source_path"], "docs/next.md");
        assert_eq!(report["spec_index"][collection][2]["source_anchor"], "fixture");
    }
    assert_eq!(report["spec_index"]["requirements"][0]["summary"], "fixture");
    assert_eq!(report["spec_index"]["requirements"][1]["summary"], "appended obligation");
    assert_eq!(report["spec_index"]["behaviors"][2]["acceptance_cases"], serde_json::json!(["A01", "A03"]));
    assert_eq!(report["unmet_requirement_count"].as_u64(), Some(3));
    assert_eq!(report["case_status_counts"]["unproven"].as_u64(), Some(3));
    assert_eq!(report["evidence_inventory"]["complete"].as_bool(), Some(true));
    assert!(report["evidence_inventory"]["entries"].as_array().is_some_and(Vec::is_empty));

    // Neither the inventory nor explicit Markdown route may infer an owner
    // from the preceding document, a later anchor or the source line.
    fs::write(&next, "| A03 | unowned case | condition |\n<a id=\"appended\"></a>\n")?;
    let invalid_out = root.path().join("invalid-report.json");
    let run = report_command(&inventory, &evidence, &invalid_out).output()?;
    assert_eq!(run.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("InvalidOwner"), "{stderr}");
    assert!(stderr.contains("docs/next.md"), "{stderr}");
    assert!(!invalid_out.exists());
    let run = report_command(&next, &evidence, &invalid_out).output()?;
    assert_eq!(run.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&run.stderr).contains("InvalidOwner"));
    assert!(!invalid_out.exists());

    fs::write(&next, "<a id=\"fixture\"></a>\n| A03 | ambiguous case | condition |\n<span name='fixture'></span>\n")?;
    let run = report_command(&inventory, &evidence, &invalid_out).output()?;
    assert_eq!(run.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("InvalidOwner"), "{stderr}");
    assert!(stderr.contains("definitions: 2"), "{stderr}");
    assert!(!invalid_out.exists());
    Ok(())
}

#[test]
fn cli_discloses_a_rejected_long_run_report_without_dropping_its_windows() -> std::io::Result<()> {
    let root = tempfile::tempdir()?;
    let spec = root.path().join("SPEC.md");
    fs::write(&spec, SPEC_TEXT).unwrap();
    let evidence = root.path().join("evidence");
    fs::create_dir(&evidence)?;
    let sampler = root.path().join("sampler.json");
    let bytes = serde_json::to_vec(&sealed_run_report()).unwrap();
    fs::write(&sampler, &bytes).unwrap();
    // The caller binds the report by a digest it does not have: the bytes are
    // never read as this observation.
    let binding = format!("{}={}", sampler.to_str().unwrap(), "0".repeat(64));
    let out = root.path().join("report.json");
    let run = report_command(&spec, &evidence, &out)
        .arg("--long-run")
        .arg(&binding)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains("long-run duration: unobserved"), "{stdout}");
    assert!(stdout.contains("rejected reports 1"), "{stdout}");
    // Silence would make `unobserved` indistinguishable from "nothing offered".
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("was not admitted: report SHA-256 mismatch"),
        "{stderr}"
    );
    let report = read_report(&out);
    let rejection = report["long_run_observation"]["rejections"]
        .get(sampler.to_str().unwrap())
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("no rejection for {}", sampler.display()));
    assert!(rejection.contains("SHA-256 mismatch"), "{rejection}");
    // The rejected report's declarations stay in the denominator, unobserved.
    assert_eq!(
        report["long_run_observation"]["windows"]["7d"].as_bool(),
        Some(false)
    );
    assert_eq!(
        report["long_run_observation"]["windows"]["30d"].as_bool(),
        Some(false)
    );
    assert_eq!(
        report["long_run_observation"]["duration_status"].as_str(),
        Some("unobserved")
    );
    Ok(())
}
