//! Model discovery of the real read-only fleet handler; see docs/specs/remote-seats.md.
//! Uses the product sandbox, provider, dispatcher, parser and committed T2 results.
//! Set SHENKO_USE_LOCAL_MODEL=1, SHENKO_LOCAL_MODEL_URL, SHENKO_LOCAL_MODEL,
//! and SHENKO_SANDBOX_ROOT to retain the original Store/CAS and every trial.

use anyhow::{Context, Result, ensure};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};
use shenko_awareness_core::CognitionCatalog;
use shenko_broker::broker::{Broker, BrokerHandle};
use shenko_exec_host::{CommittedTrialOutput, PreparedSandbox, SandboxRuntime, run_prepared_sandbox};
use shenko_exec_ssh::{SeatFleetHandler, seat::runtime::SeatRuntime};
use shenko_kernel::Supervisor;
use shenko_node::Node;
use shenko_store::{SeatLocation, Store};
use shenko_subject::{NativeChildService, NativeTaskHandlerPlugin};
use shenko_kernel::task::NativeTaskHandler;
use shenko_types::{
    ids::EntityId,
    object::{Digest, ObjectRef},
    operation::EffectStatus,
    subject::SubjectStep,
    task::{NativeTaskDefinition, TaskOutcome, TaskResult},
};
use std::{collections::BTreeSet, io::Write, path::{Path, PathBuf}, sync::Arc};
use tokio::time::{Duration, Instant};

const INVENTORY_MESSAGE: &str = "先看看已经登记的外部编码工作位，告诉我有哪些、分别属于哪个任务，还有多少问题在等决定。现在只了解现状，别启动或连接新的工作位。";
const UNRELATED_MESSAGE: &str = "这轮只计算 17 加 25，把结果告诉我。不需要查询环境或任务，也不要访问外部工作位。";

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Condition { Absent, Installed, Unrelated }

impl Condition {
    fn label(self) -> &'static str {
        match self { Self::Absent => "absent", Self::Installed => "installed", Self::Unrelated => "unrelated" }
    }
}

#[derive(Serialize)]
struct FleetObservation {
    task_id: EntityId,
    operation_id: EntityId,
    original_input_digest: Digest,
    frozen_input_ref: ObjectRef,
    receipt_ref: ObjectRef,
    complete_snapshot: bool,
}

struct Runtime {
    installed: bool,
    registered: Mutex<BTreeSet<EntityId>>,
    observations: Mutex<Vec<FleetObservation>>,
}

#[derive(Serialize)]
struct CapturedFleet<'a> {
    schema_version: u32,
    installed: bool,
    registered_seats: &'a BTreeSet<EntityId>,
    observations: &'a [FleetObservation],
    input_plan_ref: &'a ObjectRef,
    model_result_ref: &'a Option<ObjectRef>,
    continuation_step_id: EntityId,
}

impl SandboxRuntime for Runtime {
    async fn install(&self, supervisor: &Supervisor, sandbox: &PreparedSandbox, _catalog: &Arc<CognitionCatalog>) -> Result<()> {
        let store = Arc::clone(sandbox.store());
        let (head, _) = store.load_subject(sandbox.agent_id())?;
        for index in 0..2 {
            let id = EntityId::new();
            store.register_seat(id, head.subject_root_task_id, "omp_rpc", "1", SeatLocation::OmpRpc {
                host: "sandbox".into(), remote_session_id: format!("retained-work-position-{index}"),
            })?;
            self.registered.lock().insert(id);
        }
        if self.installed {
            let runtime = Arc::new(SeatRuntime::new(Arc::clone(&store)));
            let handler: Arc<dyn NativeTaskHandler> = Arc::new(SeatFleetHandler::new(store, runtime));
            supervisor.spawn(NativeTaskHandlerPlugin, handler).await?;
        }
        let native = supervisor.context().try_service::<NativeChildService>()?;
        ensure!(native.has_handler("seat.fleet", 1) == self.installed, "fleet publication does not match the trial condition");
        Ok(())
    }

    fn accept_result(&self, store: &Store, definition: &NativeTaskDefinition, result: &TaskResult) -> Result<()> {
        ensure!(definition.kind == "seat.fleet" && definition.definition_version == 1,
            "the trial consumed an unrelated delegated task");
        ensure!(result.outcome == TaskOutcome::Succeeded, "fleet task failed: {:?}", result.outcome);
        let operation = store.external_operation(result.operation_id)?;
        ensure!(operation.task_id == result.task_id && operation.operation_name == "seat.fleet.v1"
            && operation.effect_status == EffectStatus::None, "consumed result is not the read-only fleet operation");
        let attempt = store.external_operation_attempt(result.operation_id)?;
        let receipt = store.external_evidence(attempt.final_receipt_id.context("fleet has no original E1")?)?;
        let bytes = store.cas.read_object(&receipt.raw_ref.digest)?;
        let payload: Value = serde_json::from_slice(&bytes)?;
        let rows = payload["seats"].as_array().context("fleet receipt has no seat array")?;
        let actual: BTreeSet<EntityId> = rows.iter().map(|row| {
            row["seat_id"].as_str().context("seat has no stable identity")?.parse().map_err(Into::into)
        }).collect::<Result<_>>()?;
        let complete_snapshot = actual.eq(&*self.registered.lock())
            && payload["truncated"].as_bool() == Some(false);
        self.observations.lock().push(FleetObservation {
            task_id: result.task_id, operation_id: result.operation_id,
            original_input_digest: Digest::blake3(&serde_json::to_vec(&definition.input)?),
            frozen_input_ref: operation.final_input_ref, receipt_ref: receipt.raw_ref, complete_snapshot,
        });
        Ok(())
    }

    async fn capture(&self, sandbox: &PreparedSandbox, step: &SubjectStep, output: &CommittedTrialOutput) -> Result<Option<ObjectRef>> {
        let registered = self.registered.lock();
        let observations = self.observations.lock();
        let bytes = serde_json::to_vec(&CapturedFleet {
            schema_version: 1, installed: self.installed,
            registered_seats: &registered, observations: &observations,
            input_plan_ref: &output.input_plan_ref, model_result_ref: &step.model_result_ref,
            continuation_step_id: output.step_id,
        })?;
        Ok(Some(sandbox.store().cas.seal_object(&bytes, "application/json", "seat-fleet-model-observation", "original", 1)?.object))
    }
}

#[derive(Serialize)]
struct Trial {
    condition: Condition,
    sample: usize,
    installation: Option<PathBuf>,
    completed: bool,
    fleet_results: usize,
    complete_snapshot: bool,
    facts_ref: Option<ObjectRef>,
    error: Option<String>,
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = std::fs::OpenOptions::new().create_new(true).write(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

async fn measure(handle: &BrokerHandle, root: &Path, trials: &mut [Trial], seconds: u64) -> Result<()> {
    let catalog = Arc::new(CognitionCatalog::new(Arc::new(Mutex::new(Node::open(&root.join("catalog"))?))));
    for trial in trials {
        let prepared = PreparedSandbox::fresh(&format!("{}-{}", trial.condition.label(), trial.sample), root)?;
        trial.installation = Some(prepared.installation().to_path_buf());
        let runtime = Runtime { installed: trial.condition != Condition::Absent,
            registered: Mutex::new(BTreeSet::new()), observations: Mutex::new(Vec::new()) };
        let message = if trial.condition == Condition::Unrelated { UNRELATED_MESSAGE } else { INVENTORY_MESSAGE };
        let outcome = run_prepared_sandbox(prepared, handle, Arc::clone(&catalog), message,
            Instant::now() + Duration::from_secs(seconds), &runtime).await;
        let execution = match outcome {
            Ok(execution) => execution,
            Err(error) => {
                trial.error = Some(format!("{error:#}"));
                return Err(error);
            }
        };
        write_json(&trial.installation.as_ref().unwrap().join("committed-fleet-trial.json"), &execution.committed)?;
        let observations = runtime.observations.lock();
        trial.completed = true;
        trial.fleet_results = observations.len();
        trial.complete_snapshot = observations.iter().any(|observation| observation.complete_snapshot);
        trial.facts_ref = execution.facts_ref;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn fleet_discovery_is_measured_through_the_real_subject_and_original_receipts() -> Result<()> {
    if std::env::var("SHENKO_USE_LOCAL_MODEL").as_deref() != Ok("1") {
        eprintln!("SKIP seat_fleet_behavior: set SHENKO_USE_LOCAL_MODEL=1 and the served endpoint/model");
        return Ok(());
    }
    ensure!(std::env::var("SHENKO_LOCAL_MODEL_URL").is_ok() && std::env::var("SHENKO_LOCAL_MODEL").is_ok(),
        "explicit endpoint and model are required for a reported model run");
    let samples: usize = std::env::var("SHENKO_BEHAVIOR_SAMPLES").unwrap_or_else(|_| "3".into()).parse()?;
    let min_rate: f64 = std::env::var("SHENKO_BEHAVIOR_MIN_RATE").unwrap_or_else(|_| "0.85".into()).parse()?;
    let seconds: u64 = std::env::var("SHENKO_BEHAVIOR_TIMEOUT_SECS").unwrap_or_else(|_| "180".into()).parse()?;
    ensure!(samples >= 2 && seconds > 0 && min_rate.is_finite() && (0.0..=1.0).contains(&min_rate),
        "invalid finite behavior trial settings");
    let base = std::env::var_os("SHENKO_SANDBOX_ROOT").map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/seat-fleet-behavior"));
    std::fs::create_dir_all(&base)?;
    let root = base.join(format!("run-{}", EntityId::new()));
    std::fs::create_dir(&root)?;
    let mut trials: Vec<Trial> = [Condition::Absent, Condition::Installed, Condition::Unrelated].into_iter()
        .flat_map(|condition| (0..samples).map(move |sample| Trial {
            condition, sample, installation: None, completed: false, fleet_results: 0,
            complete_snapshot: false, facts_ref: None, error: None,
        })).collect();
    write_json(&root.join("planned-trials.json"), &trials)?;
    let (broker, handle) = Broker::new()?;
    let model = handle.binding().model().to_owned();
    let broker_task = tokio::spawn(broker.run());
    let mut outcome = measure(&handle, &root, &mut trials, seconds).await;
    let shutdown = match tokio::time::timeout(Duration::from_secs(120), handle.quiesce_for_handoff()).await {
        Ok(Ok(snapshot)) => json!(snapshot),
        other => {
            let message = format!("broker retirement did not complete: {other:?}");
            outcome = Err(match outcome { Ok(()) => anyhow::anyhow!(message), Err(error) => error.context(message) });
            json!({"local_provider_readers_drained":false,"remote_termination_claimed":false})
        }
    };
    broker_task.abort();
    if let Err(error) = broker_task.await {
        if !error.is_cancelled() {
            outcome = Err(match outcome { Ok(()) => error.into(), Err(primary) => primary.context(error) });
        }
    }
    write_json(&root.join("broker-shutdown.json"), &shutdown)?;
    let installed = trials.iter().filter(|trial| trial.condition == Condition::Installed && trial.complete_snapshot).count();
    let absent = trials.iter().filter(|trial| trial.condition == Condition::Absent && trial.complete_snapshot).count();
    let unrelated = trials.iter().filter(|trial| trial.condition == Condition::Unrelated && trial.completed && trial.fleet_results == 0).count();
    write_json(&root.join("report.json"), &json!({
        "schema_version":1,"model":model,"samples_per_condition":samples,"minimum_rate":min_rate,
        "installed_complete_snapshots":installed,"absent_complete_snapshots":absent,
        "unrelated_without_fleet":unrelated,"trials":trials,
        "fatal_error":outcome.as_ref().err().map(|error|format!("{error:#}")),
        "scope":"real handler discovery, complete inventory receipt and committed consumption; final answer semantics, remote agent control and long-term reliability are not scored"
    }))?;
    eprintln!("fleet model evidence: {}", root.join("report.json").display());
    outcome?;
    ensure!(installed as f64 / samples as f64 >= min_rate && installed > absent,
        "installed inventory discovery did not exceed baseline at the declared rate");
    ensure!(unrelated as f64 / samples as f64 >= min_rate, "the unrelated scenario overused fleet inspection");
    Ok(())
}
