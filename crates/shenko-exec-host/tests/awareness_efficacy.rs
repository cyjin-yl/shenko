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
    let mut cases = vec![Scenario {
        name: "network_change_is_checked_not_assumed",
        unit_id: "awareness.network-change",
        message: "刚才还能上网，现在什么都打不开了，你说这是怎么回事",
        rubric: "模型是否把“自己此刻仍然在线”当成需要一次真实观察才能成立的假设，而不是直接给出原因或结论？",
        accepted: false,
    }];
    cases.extend(CAPABILITY_GAP_CASES.iter().map(|(name, message)| Scenario {
        name, unit_id: "awareness.capability-gap", message,
        rubric: CAPABILITY_GAP_RUBRIC, accepted: false,
    }));
    cases
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
    let mut batches = Vec::new();
    for case in &cases {
        let revision = &units.iter().find(|unit| unit.unit_id == case.unit_id)
            .ok_or_else(|| anyhow::anyhow!("seed {} is missing", case.unit_id))?.revision;
        // Separate calls let held-outs start even when another sample stalls.
        // Each candidate/control pair still shares its production deadline.
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
        if failed * 5 > attempted {
            failures.push(format!("{}: {failed}/{attempted} attempts failed before scoring", case.name));
        }
        if case.accepted && (candidate.rate_over_scored < min_rate
            || candidate.rate_over_scored <= control.rate_over_scored) {
            failures.push(format!("{}: accepted unit failed rate/baseline comparison", case.name));
        }
        eprintln!("{}: candidate {}/{}, control {}/{}, failed {failed}/{attempted}, accepted={}",
            case.name, candidate.effective, candidate.scored, control.effective, control.scored, case.accepted);
        summaries.push(serde_json::json!({
            "scenario": case.name, "unit_id": case.unit_id, "accepted": case.accepted,
            "candidate": candidate, "control": control,
            "discriminating": candidate.rate_over_scored > control.rate_over_scored,
        }));
    }
    let report = serde_json::json!({
        "model": handle.binding().model(), "trials_per_condition": trials, "min_rate": min_rate,
        "sandbox_root": root, "scenarios": summaries, "batches": batches,
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
