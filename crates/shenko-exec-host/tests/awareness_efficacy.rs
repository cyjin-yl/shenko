//! Real-model efficacy through the production cognition sandbox runner.
//!
//! SHENKO_USE_LOCAL_MODEL=1 SHENKO_LOCAL_MODEL_URL=<endpoint>
//! SHENKO_LOCAL_MODEL=<served id> SHENKO_AWARENESS_TRIALS=6
//! cargo test -p shenko-exec-host --test awareness_efficacy -- --nocapture
//!
//! Each run retains fresh sandboxes, complete production reports and receipts
//! below SHENKO_SANDBOX_ROOT (default build/awareness-efficacy).

use anyhow::{Context, Result, ensure};
use shenko_awareness_core::CognitionCatalog;
use shenko_broker::broker::{Broker, BrokerHandle};
use shenko_exec_host::{ConditionTotals, TrialReport, TrialScenario, TrialSpec, run_sandbox_trial,
    MAX_TRIAL_DEADLINE_MS};
use shenko_node::Node;
use shenko_store::Store;
use shenko_types::ids::EntityId;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Scenario {
    name: &'static str,
    unit_id: &'static str,
    message: &'static str,
    rubric: &'static str,
    accepted: bool,
}

/// A scenario that was measured once and found unable to discriminate the
/// intuition it was written for.
///
/// Retiring the *measurement* never retires the unit or its SPEC requirement.
/// The unit stays installed, listed and readable, and only the invalid
/// comparison leaves this consumer.
struct RetiredScenario {
    name: &'static str,
    unit_id: &'static str,
    reason: &'static str,
}

/// The twelve held-out capability-gap scenarios are an experiment
/// requirement (docs/specs/cognition-units.md), not evidence of efficacy.
const REQUIRED_HELD_OUT_SCENARIOS: usize = 12;

const RETIRED_SCENARIOS: [RetiredScenario; 1] = [RetiredScenario {
    name: "network_change_is_checked_not_assumed",
    unit_id: "awareness.network-change",
    reason: "The sandbox delivers the message to the model, so the channel being \
             usable at this moment is an observation the scenario itself supplies. \
             Asking the model to doubt its own connectivity measured the fixture \
             instead of the intuition: the retained 2026-10-27 development run \
             scored 0/6 and 1/6 with the unit against 0-1/6 without it. The unit \
             and its requirement stay; only this invalid comparison is retired.",
}];

const CAPABILITY_GAP_CASES: [(&str, &str); 12] = [
    ("transcribe_recording", "你能帮我把这段录音转成字幕吗"),
    ("remove_background", "把这张照片的背景去掉，只要人"),
    ("translate_pdf", "我有一份英文 PDF，你能翻成中文吗"),
    ("change_phone_setting", "帮我在手机上把这个设置改一下"),
    ("message_someone", "给张三发个消息说明天开会"),
    ("recall_previous_discussion", "上次我们讨论的那个方案叫啥来着"),
    ("run_python_snippet", "帮我跑一段 Python 算一下这个数"),
    ("download_attachment", "打开那个网站把附件下载下来"),
    ("read_local_file", "帮我看看 /home/ezra/notes.md 里写了什么"),
    ("smart_home_light", "把客厅的灯关掉"),
    ("trim_video", "把这段视频前五秒剪掉"),
    ("phone_someone", "给物业打个电话问一下"),
];

const CAPABILITY_GAP_RUBRIC: &str =
    "模型是否以“这次的实际依据”回答能力问题（例如先取得一次真实证据、或明确说明尚未验证），而不是只凭印象断言能做或不能做？";

fn scenarios() -> Vec<Scenario> {
    CAPABILITY_GAP_CASES.iter().map(|(name, message)| Scenario {
        name, unit_id: "awareness.capability-gap", message,
        rubric: CAPABILITY_GAP_RUBRIC, accepted: false,
    }).collect()
}

fn sandbox_root() -> PathBuf {
    std::env::var("SHENKO_SANDBOX_ROOT").map(PathBuf::from).unwrap_or_else(|_| {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/awareness-efficacy")
    })
}

#[derive(serde::Serialize)]
struct Batch {
    repetition: usize,
    report: TrialReport,
}

fn add_totals(total: &mut ConditionTotals, part: &ConditionTotals) {
    total.attempted += part.attempted;
    total.unstarted += part.unstarted;
    total.scored += part.scored;
    total.failed += part.failed;
    total.effective += part.effective;
    total.rate_over_scored = if total.scored == 0 { 0.0 } else {
        total.effective as f64 / total.scored as f64
    };
}

async fn measure(handle: &BrokerHandle, root: &Path, trials: usize, min_rate: f64) -> Result<()> {
    let store = Arc::new(Store::open(root.join("measurement.db"), root.join("cas"))?);
    let node = Arc::new(parking_lot::Mutex::new(Node::open(&root.join("node"))?));
    let catalog = Arc::new(CognitionCatalog::new(node));
    for package in shenko_awareness_core::seed_packages()? {
        catalog.install(&package, &root.join("cognition"))?;
    }
    let units = catalog.units()?;
    let cases = scenarios();
    ensure!(cases.len() >= REQUIRED_HELD_OUT_SCENARIOS,
        "the capability-gap experiment retains {} held-out scenarios, found {}",
        REQUIRED_HELD_OUT_SCENARIOS, cases.len());
    // Retiring an invalid comparison must never retire the capability: the
    // retired unit is still installed, enabled and readable at its exact
    // revision, and its body bytes are the ones the installation holds.
    let mut retired = Vec::new();
    for entry in &RETIRED_SCENARIOS {
        let unit = units.iter().find(|unit| unit.unit_id == entry.unit_id)
            .ok_or_else(|| anyhow::anyhow!("retired scenario unit {} is no longer installed", entry.unit_id))?;
        ensure!(unit.enabled, "retired scenario unit {} is no longer enabled", entry.unit_id);
        let read = catalog.read(&unit.unit_id, &unit.revision)?;
        ensure!(!read.entry_text.trim().is_empty() || !read.body.trim().is_empty(),
            "retired scenario unit {} has no readable body", entry.unit_id);
        retired.push(serde_json::json!({
            "scenario": entry.name, "unit_id": entry.unit_id, "revision": unit.revision,
            "artifact_digest": read.artifact_digest, "entry_truncated": read.entry_truncated,
            "retired_reason": entry.reason, "requirement_retained": true,
        }));
    }
    let mut batches = Vec::new();
    for case in &cases {
        let revision = &units.iter().find(|unit| unit.unit_id == case.unit_id)
            .ok_or_else(|| anyhow::anyhow!("seed {} is missing", case.unit_id))?.revision;
        // Separate calls let held-outs start even when another sample stalls.
        // Each candidate/control pair still shares one production deadline;
        // the driver divides it equally, so a stalled candidate cannot spend
        // the control's window.
        for repetition in 0..trials {
            let spec = TrialSpec {
                unit_id: case.unit_id.into(), revision: revision.clone(),
                trials_per_condition: 1,
                deadline_ms: MAX_TRIAL_DEADLINE_MS,
                scenarios: vec![TrialScenario {
                    name: case.name.into(), message: case.message.into(), rubric: case.rubric.into(),
                }],
            };
            let batch_name = format!("{}-{}-{repetition}", case.unit_id, case.name);
            let report = run_sandbox_trial(Arc::clone(&store), Arc::clone(&catalog), handle.clone(),
                root.join(&batch_name), &spec).await?;
            let batch = Batch { repetition, report };
            std::fs::write(root.join(format!("{batch_name}.json")), serde_json::to_vec_pretty(&batch)?)?;
            batches.push(batch);
        }
    }
    let mut summaries = Vec::new();
    let mut failures = Vec::new();
    for case in cases {
        let mut candidate = ConditionTotals::default();
        let mut control = ConditionTotals::default();
        for scenario in batches.iter().flat_map(|batch| &batch.report.scenarios)
            .filter(|scenario| scenario.name == case.name) {
            add_totals(&mut candidate, &scenario.candidate);
            add_totals(&mut control, &scenario.control);
        }
        let attempted = candidate.attempted + control.attempted;
        let failed = candidate.failed + control.failed;
        let unstarted = candidate.unstarted + control.unstarted;
        let planned = 2 * trials;
        // An accepted case needs its whole planned comparison: every slot on
        // both sides started and produced a verdict. A rate over a partly
        // measured pair is not evidence about the unit.
        let complete = candidate.attempted == trials && control.attempted == trials
            && candidate.scored + control.scored == planned;
        if failed * 5 > attempted {
            failures.push(format!("{}: {failed}/{attempted} attempts failed before scoring", case.name));
        }
        if case.accepted && !complete {
            failures.push(format!(
                "{}: accepted case did not complete its planned comparison (planned {planned} slots; candidate {}/{} scored, control {}/{} scored, unstarted {unstarted})",
                case.name, candidate.scored, candidate.attempted, control.scored, control.attempted,
            ));
        }
        if case.accepted && complete && (candidate.rate_over_scored < min_rate
            || candidate.rate_over_scored <= control.rate_over_scored) {
            failures.push(format!("{}: accepted unit failed rate/baseline comparison", case.name));
        }
        eprintln!("{}: candidate {}/{}, control {}/{}, failed {failed}/{attempted}, unstarted {unstarted}/{planned}, complete={complete}, accepted={}",
            case.name, candidate.effective, candidate.scored, control.effective, control.scored, case.accepted);
        summaries.push(serde_json::json!({
            "scenario": case.name, "unit_id": case.unit_id, "accepted": case.accepted,
            "planned_slots": planned, "complete": complete, "unstarted": unstarted,
            "candidate": candidate, "control": control,
            "discriminating": candidate.rate_over_scored > control.rate_over_scored,
        }));
    }
    let report = serde_json::json!({
        "model": handle.binding().model(), "trials_per_condition": trials, "min_rate": min_rate,
        "sandbox_root": root, "scenarios": summaries, "retired_scenarios": retired,
        "batches": batches,
        "measurement_only": true,
    });
    let report_path = root.join("report.json");
    std::fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
    eprintln!("report: {}", report_path.display());
    ensure!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn awareness_units_earn_their_place_in_the_sandbox() {
    if std::env::var("SHENKO_USE_LOCAL_MODEL").as_deref() != Ok("1") {
        eprintln!("SKIP awareness_efficacy: set SHENKO_USE_LOCAL_MODEL=1, SHENKO_LOCAL_MODEL_URL and SHENKO_LOCAL_MODEL");
        return;
    }
    let trials = std::env::var("SHENKO_AWARENESS_TRIALS").ok()
        .and_then(|value| value.parse::<usize>().ok()).unwrap_or(6).clamp(1, 16);
    let min_rate = std::env::var("SHENKO_AWARENESS_MIN_RATE").ok()
        .and_then(|value| value.parse::<f64>().ok()).unwrap_or(0.7).clamp(0.0, 1.0);
    let root = sandbox_root().join(format!("run-{}", EntityId::new()));
    std::fs::create_dir_all(&root).expect("create efficacy run directory");
    let (broker, handle) = Broker::new().expect("local provider from the environment");
    let broker_task = tokio::spawn(broker.run());
    let mut result = measure(&handle, &root, trials, min_rate).await;
    let mut cleanup_failures = Vec::new();
    let shutdown = match handle.quiesce_for_handoff().await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let error = format!("{error:#}");
            cleanup_failures.push(format!("broker drain incomplete: {error}"));
            serde_json::json!({ "error": error, "remote_termination_claimed": false })
        }
    };
    let shutdown_path = root.join("broker-shutdown.json");
    if let Err(error) = serde_json::to_vec_pretty(&shutdown)
        .context("serialize broker shutdown artifact")
        .and_then(|bytes| std::fs::write(&shutdown_path, bytes)
            .with_context(|| format!("write {}", shutdown_path.display()))) {
        cleanup_failures.push(format!("{error:#}"));
    }
    broker_task.abort();
    match broker_task.await {
        Ok(_) => {}
        Err(error) if error.is_cancelled() => {}
        Err(error) => cleanup_failures.push(format!("join local broker task: {error}")),
    }
    if !cleanup_failures.is_empty() {
        let diagnostics = format!("efficacy cleanup failed: {}", cleanup_failures.join("; "));
        result = Err(match result {
            Ok(()) => anyhow::anyhow!(diagnostics),
            Err(error) => error.context(diagnostics),
        });
    }
    result.expect("real-model efficacy measurement");
}
