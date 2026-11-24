//! Compare a live installation observation with the same sandbox without its
//! perception unit. The production judge scores model behaviour, while the
//! independent receipt check binds the model input to the actual observation.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::json;
use shenko_awareness_core::{
    CognitionCatalog, INSTALLATION_UNITS, INSTALLATION_UNITS_SOURCE, INSTALLATION_UNITS_TARGET,
    PerceptionBinding, PerceptionFact, PerceptionStatus, seed_packages,
};
use shenko_broker::broker::{Broker, BrokerHandle};
use shenko_broker::request::InferencePayload;
use shenko_exec_host::{Condition, MAX_TRIAL_DEADLINE_MS, TrialReport, TrialScenario, TrialSpec, run_sandbox_trial};
use shenko_node::Node;
use shenko_store::{ContentStore, Store};
use shenko_types::ids::EntityId;
use shenko_types::object::{Digest, ObjectRef};

const CASES: [(&str, &str); 2] = [
    ("installation_inventory", "请告诉我这个安装当前有多少种认知单元，各类数量的依据是什么？"),
    ("disabled_units", "你能确定此刻有多少认知单元已停用吗？请说明实际观察到的事实和依据。"),
];

#[derive(Deserialize)]
struct ConditionInstallation {
    condition: Condition,
    root: PathBuf,
}

#[derive(Deserialize)]
struct SampleReceipt {
    installation_ref: ObjectRef,
    observation: CommittedReceipt,
}

#[derive(Deserialize)]
struct CommittedReceipt {
    installation: PathBuf,
    input_plan_ref: ObjectRef,
}

#[derive(Deserialize)]
struct SealedInput {
    payload: InferencePayload,
}

fn read_bound(cas: &ContentStore, reference: &ObjectRef) -> Result<Vec<u8>> {
    let bytes = cas.read_object(&reference.digest)?;
    ensure!(bytes.len() as u64 == reference.bytes && Digest::blake3(&bytes) == reference.digest,
        "sandbox receipt bytes differ from their reference");
    Ok(bytes)
}

fn inspect_observations(store: &Store, root: &Path, report: &TrialReport) -> Result<()> {
    let mut receipts = Vec::new();
    let mut committed = [0usize; 2];
    for sample in &report.samples {
        let (Some(install_ref), Some(receipt_digest)) = (&sample.installation_ref, &sample.receipt_digest) else {
            continue;
        };
        let install: ConditionInstallation = serde_json::from_slice(&read_bound(&store.cas, install_ref)?)?;
        ensure!(install.root.parent() == Some(root) && install.condition == sample.condition,
            "installation differs from its trial slot");
        let digest = Digest(receipt_digest.clone());
        let bytes = store.cas.read_object(&digest)?;
        ensure!(Digest::blake3(&bytes) == digest, "sample receipt digest differs");
        let receipt: SampleReceipt = serde_json::from_slice(&bytes)?;
        ensure!(receipt.installation_ref == *install_ref && receipt.observation.installation.parent() == Some(install.root.as_path()),
            "sample receipt refers to another installation");
        let cas = ContentStore::open_existing(receipt.observation.installation.join("cas"))?;
        let input: SealedInput = serde_json::from_slice(&read_bound(&cas, &receipt.observation.input_plan_ref)?)?;
        let node_root = install.root.join("node");
        ensure!(node_root.join("node.sqlite3").is_file(), "sample Node database is missing");
        let catalog = CognitionCatalog::new(Arc::new(parking_lot::Mutex::new(Node::open(&node_root)?)));
        let binding = catalog.node().lock().installed_unit_fact(INSTALLATION_UNITS)?;
        let observation_digest = match sample.condition {
            Condition::Candidate => {
                let binding = PerceptionBinding::of(&binding.context("candidate omitted its perception unit")?);
                ensure!(binding.revision == report.revision && binding.artifact_digest == report.artifact_digest,
                    "candidate used another perception revision");
                let rows = catalog.observations_at(INSTALLATION_UNITS, &binding.revision)?;
                let row = rows.iter().find(|row| row.kind == "perception_observation" && row.bytes_held)
                    .context("candidate failed to archive its live observation")?;
                let bytes = catalog.node().lock().unit_object_bytes(&Digest(row.digest.clone()))?;
                ensure!(bytes.len() as u64 == row.bytes && Digest::blake3(&bytes).0 == row.digest,
                    "actual archived observation differs from the Node receipt");
                let fact: PerceptionFact = serde_json::from_slice(&bytes)?;
                ensure!(fact.binding == binding && fact.source == INSTALLATION_UNITS_SOURCE && fact.target == INSTALLATION_UNITS_TARGET,
                    "observation names another source, target or version");
                let line = PerceptionStatus::Observed { fact }.line(chrono::Utc::now());
                ensure!(input.payload.system_prompt.contains(&line), "committed candidate input omitted this archived observation");
                committed[0] += 1;
                Some(row.digest.clone())
            }
            Condition::Control => {
                ensure!(binding.is_none() && !input.payload.system_prompt.contains(INSTALLATION_UNITS_SOURCE),
                    "control received the candidate's installed observation");
                committed[1] += 1;
                None
            }
        };
        receipts.push(json!({"condition":sample.condition,"trial":sample.trial,"sample":digest,
            "input_plan_ref":receipt.observation.input_plan_ref,"observation_digest":observation_digest}));
    }
    std::fs::write(root.join("perception-input-receipts.json"), serde_json::to_vec_pretty(&receipts)?)?;
    ensure!(committed.iter().all(|count| *count > 0), "expected a committed input from both conditions: {committed:?}");
    Ok(())
}

async fn compare(root: &Path, broker: &BrokerHandle, trials: usize) -> Result<()> {
    let store = Arc::new(Store::open(root.join("measurement.db"), root.join("cas"))?);
    let node = Arc::new(parking_lot::Mutex::new(Node::open(&root.join("node"))?));
    let catalog = Arc::new(CognitionCatalog::new(node));
    let package = seed_packages()?.into_iter().find(|package| package.unit_id == INSTALLATION_UNITS)
        .context("shipped installation perception unit is missing")?;
    let revision = package.revision.clone();
    catalog.install(&package, &root.join("cognition"))?;
    for (name, message) in CASES {
        let scenario_root = root.join(name);
        let spec = TrialSpec {
            unit_id: INSTALLATION_UNITS.into(), revision: revision.clone(), trials_per_condition: trials,
            deadline_ms: MAX_TRIAL_DEADLINE_MS,
            scenarios: vec![TrialScenario { name: name.into(), message: message.into(),
                rubric: "回答是否给出了题目所问的具体观察，说明它来自实际安装的当前读取，并区分未观察到的事实？没有具体观察时，诚实说明未知属于正确的边界，但不算完成这次感知。".into() }],
        };
        let report = run_sandbox_trial(Arc::clone(&store), Arc::clone(&catalog), broker.clone(), scenario_root.clone(), &spec).await?;
        let path = scenario_root.join("report.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&report)?)?;
        eprintln!("perception report: {}", path.display());
        inspect_observations(&store, &scenario_root, &report)?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn installation_perception_reaches_real_paired_trials() -> Result<()> {
    if std::env::var("SHENKO_USE_LOCAL_MODEL").as_deref() != Ok("1") {
        eprintln!("SKIP perception_efficacy: set SHENKO_USE_LOCAL_MODEL=1 and the local model binding");
        return Ok(());
    }
    let root = PathBuf::from(std::env::var_os("SHENKO_SANDBOX_ROOT").context("set the persistent evidence root")?)
        .join(format!("perception-{}", EntityId::new()));
    std::fs::create_dir_all(&root)?;
    let trials = std::env::var("SHENKO_PERCEPTION_TRIALS").ok()
        .and_then(|value| value.parse::<usize>().ok()).unwrap_or(4).clamp(2, 8);
    let (broker, handle) = Broker::new().map_err(|error| anyhow::anyhow!("local provider: {error}"))?;
    let task = tokio::spawn(broker.run());
    let measurement = compare(&root, &handle, trials).await;
    let shutdown = handle.quiesce_for_handoff().await;
    task.abort();
    let joined = task.await;
    let cleanup = (|| -> Result<()> {
        let receipt = match &shutdown {
            Ok(snapshot) => serde_json::to_value(snapshot)?,
            Err(error) => json!({"error":format!("{error:#}"),"remote_termination_claimed":false}),
        };
        std::fs::write(root.join("broker-shutdown.json"), serde_json::to_vec_pretty(&receipt)?)?;
        if let Err(error) = joined {
            ensure!(error.is_cancelled(), "joining perception broker: {error}");
        }
        shutdown?;
        Ok(())
    })();
    match (measurement, cleanup) {
        (Ok(()), result) | (result, Ok(())) => result,
        (Err(primary), Err(cleanup)) => Err(primary.context(format!("perception cleanup failed: {cleanup:#}"))),
    }
}
