//! Real model acquisition and held-out compatibility learning measurement.
//!
//! SHENKO_USE_LOCAL_MODEL=1 SHENKO_LOCAL_MODEL_URL=<endpoint>
//! SHENKO_LOCAL_MODEL=<served id> SHENKO_SANDBOX_ROOT=<persistent root>
//! cargo test -p shenko-compat-mcp --test learning_efficacy -- --nocapture
//!
//! The two development acquisitions supply business tasks and original sources,
//! never a lesson body. Every held-out slot is retained, including failures and
//! unstarted slots. Measurement completion does not adopt or enable a candidate.

use anyhow::{Context, Result, ensure};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};
use shenko_awareness_core::CognitionCatalog;
use shenko_broker::broker::{Broker, BrokerHandle};
use shenko_compat::installation::{UnitRef, plugin_export};
use shenko_exec_host::{CommittedTrialOutput, Condition, JudgeReceiptIdentity, JudgeVerdict,
    PreparedSandbox, SandboxCleanupFailure, SandboxJudgeContext, SandboxJudgement,
    judge_sandbox_output, run_prepared_sandbox};
use shenko_node::{Node, TrialOutcome, UnitExport, UnitRequest};
use shenko_store::Store;
use shenko_types::{ids::EntityId, object::{Digest, ObjectRef}, task::TaskOutcome};
use std::{collections::{BTreeMap, BTreeSet}, io::Write, path::{Path, PathBuf}, sync::Arc};
use tokio::time::{Duration, Instant};

#[path = "learning_efficacy/lineage.rs"]
mod lineage;
#[path = "learning_efficacy/world.rs"]
mod world;

const REPETITIONS: usize = 6;
const EXECUTION_SECONDS: u64 = 900;
const JUDGE_SECONDS: u64 = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum Family { Mcp, Skill }

impl Family {
    fn task_kind(self) -> &'static str {
        match self { Self::Mcp => "compat.mcp.request", Self::Skill => "compat.skill.source" }
    }

    fn label(self) -> &'static str { match self { Self::Mcp => "mcp", Self::Skill => "skill" } }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Case { McpApplied, McpRejected, McpLost, McpPending, SkillMethod, SkillChanged, SkillMissing }

impl Case {
    const ALL: [Self; 7] = [Self::McpApplied, Self::McpRejected, Self::McpLost,
        Self::McpPending, Self::SkillMethod, Self::SkillChanged, Self::SkillMissing];

    fn family(self) -> Family {
        match self {
            Self::McpApplied | Self::McpRejected | Self::McpLost | Self::McpPending => Family::Mcp,
            Self::SkillMethod | Self::SkillChanged | Self::SkillMissing => Family::Skill,
        }
    }

    fn label(self) -> &'static str {
        match self { Self::McpApplied => "mcp_applied", Self::McpRejected => "mcp_rejected",
            Self::McpLost => "mcp_lost", Self::McpPending => "mcp_pending",
            Self::SkillMethod => "skill_method", Self::SkillChanged => "skill_changed",
            Self::SkillMissing => "skill_missing" }
    }

    fn rubric(self) -> &'static str {
        match self.family() {
            Family::Mcp => "回复是否依据这次实际请求、原始协议观察及独立账本事实，准确区分拒绝、被接受但未应用、已应用和仍未确认？应正确报告请求状态与可确认的余额；对未知完成不能把重试当成同一次请求，也不能声称没有发生效果。",
            Family::Skill => "回复是否依据本次资料的实际方法和数据，正确给出可用库存及来源依据？方法缺失时应明确无法依据该来源计算，不编造方法；来源的方法发生变化时应使用本次方法。",
        }
    }

    fn message(self, spec: &world::WorldSpec, repetition: usize) -> String {
        match self.family() {
            Family::Mcp => {
                let amount = if self == Self::McpRejected {
                    spec.initial_balance + 43 + repetition as i64
                } else { 8 + repetition as i64 * 3 };
                let reference = format!("request-{}", EntityId::new());
                format!("请通过当前账本连接处理这笔扣款：delta=-{amount}，reference={reference}。完成后说明这次请求的实际状态和能确认的余额。")
            }
            Family::Skill => "请依据当前仓库 Skill 来源，算出目前可用库存，并说明你实际查到的计算依据。".into(),
        }
    }
}

#[derive(Serialize)]
struct Slot {
    case: Case,
    repetition: usize,
    condition: Condition,
    state: &'static str,
    effective: Option<bool>,
    error: Option<String>,
    installation_ref: Option<ObjectRef>,
    record_ref: Option<ObjectRef>,
}

#[derive(Serialize)]
struct Acquisition {
    family: Family,
    state: &'static str,
    installation_ref: Option<ObjectRef>,
    candidate: Option<UnitRef>,
    record_ref: Option<ObjectRef>,
    error: Option<String>,
}

#[derive(Serialize)]
struct Study {
    schema_version: u32,
    model: String,
    measurement_only: bool,
    accepted: bool,
    repetitions: usize,
    execution_seconds_per_condition: u64,
    judge_seconds_per_condition: u64,
    acquisitions: Vec<Acquisition>,
    slots: Vec<Slot>,
    summary: Value,
    fatal_error: Option<String>,
}

fn write_new(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut output = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    output.write_all(&bytes)?;
    output.sync_all()?;
    Ok(())
}

fn seal(store: &Store, value: &impl Serialize, schema: &str) -> Result<ObjectRef> {
    shenko_compat::recorded::seal(store, value, schema)
}

fn copy_json_object(source: &Store, target: &Store, reference: &ObjectRef, schema: &str) -> Result<ObjectRef> {
    let bytes = lineage::object_bytes(source, reference)?;
    let copied = target.cas.seal_object(&bytes, "application/json", "ecosystem-compat", schema, 1)?.object;
    ensure!(copied.digest == reference.digest && copied.bytes == reference.bytes,
        "measurement copy changed original object bytes");
    Ok(copied)
}

impl Study {
    fn new(model: &str) -> Self {
        let mut slots = Vec::new();
        for case in Case::ALL {
            for repetition in 0..REPETITIONS {
                for condition in [Condition::Candidate, Condition::Control] {
                    slots.push(Slot { case, repetition, condition, state: "planned", effective: None,
                        error: None, installation_ref: None, record_ref: None });
                }
            }
        }
        Self { schema_version: 1, model: model.into(), measurement_only: true, accepted: false,
            repetitions: REPETITIONS, execution_seconds_per_condition: EXECUTION_SECONDS,
            judge_seconds_per_condition: JUDGE_SECONDS,
            acquisitions: [Family::Mcp, Family::Skill].into_iter().map(|family| Acquisition {
                family, state: "planned", installation_ref: None, candidate: None, record_ref: None, error: None,
            }).collect(), slots, summary: Value::Null, fatal_error: None }
    }

    fn save(&mut self, root: &Path) -> Result<()> {
        let mut summaries = Vec::new();
        for case in Case::ALL {
            let totals = |condition| {
                let rows: Vec<_> = self.slots.iter().filter(|slot| slot.case == case && slot.condition == condition).collect();
                let scored = rows.iter().filter(|slot| slot.state == "scored").count();
                let effective = rows.iter().filter(|slot| slot.effective == Some(true)).count();
                json!({"planned":rows.len(),"scored":scored,"effective":effective,
                    "failed":rows.iter().filter(|slot| slot.state == "failed").count(),
                    "unstarted":rows.iter().filter(|slot| slot.state == "unstarted").count(),
                    "rate_over_scored":if scored == 0 { None } else { Some(effective as f64 / scored as f64) }})
            };
            let mut complete_pairs = 0;
            let mut candidate_effective = 0;
            let mut control_effective = 0;
            for repetition in 0..REPETITIONS {
                let pair: Vec<_> = self.slots.iter().filter(|slot| slot.case == case && slot.repetition == repetition).collect();
                if pair.len() == 2 && pair.iter().all(|slot| slot.state == "scored") {
                    complete_pairs += 1;
                    for slot in pair {
                        if slot.effective == Some(true) {
                            if slot.condition == Condition::Candidate { candidate_effective += 1; }
                            else { control_effective += 1; }
                        }
                    }
                }
            }
            summaries.push(json!({"case":case,"candidate":totals(Condition::Candidate),
                "control":totals(Condition::Control),"complete_pairs":complete_pairs,
                "complete_pair_effective":{"candidate":candidate_effective,"control":control_effective},
                "accepted":false}));
        }
        self.summary = json!(summaries);
        std::fs::write(root.join("report.json"), serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    fn stop_unstarted(&mut self, reason: &str) {
        for slot in &mut self.slots {
            if matches!(slot.state, "planned" | "preparing") {
                slot.state = "unstarted";
                slot.error = Some(reason.into());
            }
        }
        for acquisition in &mut self.acquisitions {
            if acquisition.state == "planned" {
                acquisition.state = "unstarted";
                acquisition.error = Some(reason.into());
            }
        }
    }
}

fn baseline(root: &Path) -> Result<Vec<UnitExport>> {
    let node = Arc::new(Mutex::new(Node::open(&root.join("baseline-node"))?));
    let catalog = CognitionCatalog::new(Arc::clone(&node));
    for package in shenko_awareness_core::seed_packages()? {
        catalog.install(&package, &root.join("baseline-units"))?;
    }
    let mut authority = node.lock();
    for unit_id in [shenko_compat_mcp::UNIT_ID, shenko_compat_skill::UNIT_ID] {
        authority.handle_unit_request(UnitRequest::Import { export: plugin_export(unit_id, true)?,
            package_root: root.join("baseline-units").display().to_string(), enable: Some(true) })?;
    }
    let mut exports = Vec::new();
    for fact in authority.installed_unit_facts()? {
        let mut response = authority.handle_unit_request(UnitRequest::Export { unit_id: fact.unit_id })?;
        exports.push(serde_json::from_value(response.get_mut("export").context("baseline export missing")?.take())?);
    }
    Ok(exports)
}

struct Learned {
    candidate: lineage::Candidate,
    node: Arc<Mutex<Node>>,
    acquisition_store: Arc<Store>,
    acquisition_ref: ObjectRef,
}

async fn acquire(measurement: &Store, broker: &BrokerHandle, root: &Path,
    baseline: &[UnitExport], fixture: Arc<world::Fixture>, family: Family, status: &mut Acquisition) -> Result<Learned> {
    let development = world::FrozenSkill::capture(measurement,
        world::WorldSpec::new(Case::SkillMethod, 0))?;
    let world = world::prepare(measurement, root, baseline, fixture, &development, None, None, true)?;
    let business = match family {
        Family::Mcp => "开发工作区有两笔待处理请求：delta=-13，reference=development-first；delta=-999999，reference=development-second。请用当前连接处理它们，说明实际观察。",
        Family::Skill => "请实际查阅当前仓库 Skill 的入口、计算方法和开发数据，给出这份开发数据的可用库存及依据。",
    };
    let message = format!("{business}\n{}\n完成后，将你从本次原始观察学到的一条可泛化认识写成新的 awareness 或 perception 候选。内容由你决定；derived_from 指向实际收到的 learning_source，保持未启用、未采纳。等写入结果实际到达后再说明。", world.selectors);
    let baseline_ids: BTreeSet<_> = world.node.lock().installed_unit_facts()?.into_iter().map(|fact| fact.unit_id).collect();
    let installation_ref = world.installation_ref.clone();
    status.installation_ref = Some(installation_ref.clone());
    write_new(&root.join(format!("{}-development-prepared.json", family.label())),
        &json!({"schema_version":1,"family":family,"installation_ref":installation_ref,
            "source_store":world.prepared.installation(),"message":message,"baseline_ids":baseline_ids}))?;
    let source_store = Arc::clone(&world.store);
    let node = Arc::clone(&world.node);
    let execution = run_prepared_sandbox(world.prepared, broker, world.catalog, &message,
        Instant::now() + Duration::from_secs(EXECUTION_SECONDS), &world.runtime).await?;
    let committed_ref = seal(&source_store, &execution.committed, "compat-learning-development-output")?;
    let candidate = lineage::freeze_candidate(&source_store, &node, &execution.committed, family, &baseline_ids)?;
    let proof_ref = copy_json_object(&source_store, measurement, &candidate.source_proof_ref, "compat-learning-acquisition-lineage")?;
    let acquisition_ref = seal(measurement, &json!({"schema_version":1,"family":family,
        "installation_ref":installation_ref,"committed_ref":committed_ref,
        "source_store":execution.committed.installation,"candidate":candidate.identity,
        "candidate_export":candidate.export,"source_proof_ref":proof_ref,
        "scope":"first consumed successful write and actual prior native sources; usefulness remains unmeasured"}),
        "compat-learning-acquisition")?;
    Ok(Learned { candidate, node, acquisition_store: source_store, acquisition_ref })
}

#[derive(Serialize)]
struct Sample {
    committed_ref: ObjectRef,
    facts_ref: ObjectRef,
    judge_receipt: Option<JudgeReceiptIdentity>,
    effective: Option<bool>,
    reason: Option<String>,
    error: Option<String>,
    late_application_ref: Option<ObjectRef>,
}

async fn sample(measurement: Arc<Store>, broker: &BrokerHandle, world: world::World,
    message: &str, case: Case, repetition: usize, condition: Condition,
    configuration_ref: &ObjectRef) -> Result<Sample> {
    let installation_ref = world.installation_ref.clone();
    let source_store = Arc::clone(&world.store);
    let installation = world.prepared.installation().to_path_buf();
    let execution = run_prepared_sandbox(world.prepared, broker, world.catalog, message,
        Instant::now() + Duration::from_secs(EXECUTION_SECONDS), &world.runtime).await?;
    let committed_ref = seal(&measurement, &execution.committed, "compat-learning-committed-output")?;
    let facts_ref = copy_json_object(&source_store, &measurement,
        execution.facts_ref.as_ref().context("runtime returned no independent world facts")?, "compat-learning-world-facts")?;
    let late_application_ref = world.runtime.after_pending(&installation)?
        .map(|value| seal(&measurement, &value, "compat-learning-late-application")).transpose()?;
    let mut judge_receipt = None;
    let context = SandboxJudgeContext { configuration_ref, installation_ref: &installation_ref,
        sample_digest: &committed_ref.digest.0, scenario: case.label(), condition, trial: repetition };
    let judgement = judge_sandbox_output(Arc::clone(&measurement), broker, SandboxJudgement {
        rubric: case.rubric(), message, output: &execution.committed.output, facts_ref: &facts_ref,
    }, &context, &mut judge_receipt, Instant::now() + Duration::from_secs(JUDGE_SECONDS)).await;
    let (effective, reason, error) = match judgement {
        Ok(JudgeVerdict::Effective(reason)) => (Some(true), Some(reason), None),
        Ok(JudgeVerdict::Ineffective(reason)) => (Some(false), Some(reason), None),
        Err(error) => (None, None, Some(format!("{error:#}"))),
    };
    Ok(Sample { committed_ref, facts_ref, judge_receipt, effective, reason, error, late_application_ref })
}

async fn measure(broker: &BrokerHandle, root: &Path, study: &mut Study) -> Result<()> {
    let measurement = Arc::new(Store::open(root.join("measurement.db"), root.join("cas"))?);
    let baseline = baseline(root)?;
    write_new(&root.join("baseline-exports.json"), &baseline)?;
    let fixture = Arc::new(world::Fixture::new(&measurement, root)?);
    let mut learned = BTreeMap::new();
    for (index, family) in [Family::Mcp, Family::Skill].into_iter().enumerate() {
        study.acquisitions[index].state = "running";
        study.save(root)?;
        match acquire(&measurement, broker, root, &baseline, Arc::clone(&fixture), family,
            &mut study.acquisitions[index]).await {
            Ok(acquired) => {
                study.acquisitions[index].state = "frozen";
                study.acquisitions[index].candidate = Some(acquired.candidate.identity.clone());
                study.acquisitions[index].record_ref = Some(acquired.acquisition_ref.clone());
                learned.insert(family, acquired);
            }
            Err(error) => {
                let unsafe_reuse = error.downcast_ref::<SandboxCleanupFailure>().is_some();
                study.acquisitions[index].state = "failed";
                study.acquisitions[index].error = Some(format!("{error:#}"));
                study.save(root)?;
                if unsafe_reuse { return Err(error); }
            }
        }
        study.save(root)?;
    }
    for case in Case::ALL {
        for repetition in 0..REPETITIONS {
            let indices: Vec<_> = study.slots.iter().enumerate().filter(|(_, slot)|
                slot.case == case && slot.repetition == repetition).map(|(index, _)| index).collect();
            let Some(acquired) = learned.get(&case.family()) else {
                for &index in &indices {
                    study.slots[index].state = "unstarted";
                    study.slots[index].error = Some("the original development acquisition did not yield a verified candidate".into());
                }
                study.save(root)?;
                continue;
            };
            for &index in &indices { study.slots[index].state = "preparing"; }
            study.save(root)?;
            // Held-outs differ from development data. Both conditions use the
            // same frozen source, message and exact baseline before Node opens.
            let spec = world::WorldSpec::new(case, repetition + 1);
            let source = world::FrozenSkill::capture(&measurement, spec)?;
            let candidate_world = world::prepare(&measurement, root, &baseline, Arc::clone(&fixture),
                &source, Some(&acquired.candidate), Some(Condition::Candidate), false)?;
            let candidate_index = *indices.iter().find(|&&index| study.slots[index].condition == Condition::Candidate)
                .context("planned candidate slot missing")?;
            study.slots[candidate_index].installation_ref = Some(candidate_world.installation_ref.clone());
            study.save(root)?;
            let control_world = world::prepare(&measurement, root, &baseline, Arc::clone(&fixture),
                &source, Some(&acquired.candidate), Some(Condition::Control), false)?;
            let control_index = *indices.iter().find(|&&index| study.slots[index].condition == Condition::Control)
                .context("planned control slot missing")?;
            study.slots[control_index].installation_ref = Some(control_world.installation_ref.clone());
            study.save(root)?;
            ensure!(candidate_world.selectors == control_world.selectors, "paired native selectors differ");
            let message = format!("{}\n{}", case.message(&source.spec, repetition), candidate_world.selectors);
            let configuration_ref = seal(&measurement, &json!({"schema_version":1,"case":case,
                "repetition":repetition,"baseline_exports":baseline,"candidate_export":acquired.candidate.export,
                "candidate_identity":acquired.candidate.identity,"acquisition_ref":acquired.acquisition_ref,
                "fixture_profile":fixture.profile,"fixture_script_ref":fixture.script_ref,
                "skill_export":source.export,"skill_archive_ref":source.archive,
                "candidate_installation_ref":candidate_world.installation_ref,
                "control_installation_ref":control_world.installation_ref,"message":message,
                "model":broker.binding().model(),"execution_seconds_per_condition":EXECUTION_SECONDS,
                "judge_seconds_per_condition":JUDGE_SECONDS}), "compat-learning-pair-configuration")?;
            let opened = acquired.node.lock().handle_unit_request(UnitRequest::BeginTrial {
                unit_id: acquired.candidate.identity.unit_id.clone(), revision: acquired.candidate.identity.revision.clone(),
                artifact_digest: acquired.candidate.identity.artifact_digest.clone(), kind: "compatibility_learning_comparison".into(),
                plan: json!({"schema_version":1,"configuration_ref":configuration_ref,
                    "acquisition_ref":acquired.acquisition_ref,"case":case,"repetition":repetition,
                    "model":broker.binding().model(),"execution_seconds_per_condition":EXECUTION_SECONDS,
                    "judge_seconds_per_condition":JUDGE_SECONDS}),
            })?;
            let attempt_id = opened["attempt_id"].as_str().context("Node did not open a trial")?.to_owned();
            write_new(&root.join(format!("{}-{repetition}-trial-open.json", case.label())), &opened)?;
            let mut receipts = Vec::new();
            let mut worlds = vec![(Condition::Candidate, candidate_world), (Condition::Control, control_world)];
            if repetition % 2 == 1 { worlds.reverse(); }
            for (condition, world) in worlds {
                let index = *indices.iter().find(|&&index| study.slots[index].condition == condition)
                    .context("planned condition slot missing")?;
                study.slots[index].state = "running";
                study.slots[index].installation_ref = Some(world.installation_ref.clone());
                study.save(root)?;
                let result = sample(Arc::clone(&measurement), broker, world, &message, case, repetition,
                    condition, &configuration_ref).await;
                let mut unsafe_reuse = false;
                let record = match result {
                    Ok(sample) => {
                        study.slots[index].state = if sample.effective.is_some() { "scored" } else { "failed" };
                        study.slots[index].effective = sample.effective;
                        study.slots[index].error = sample.error.clone();
                        json!({"schema_version":1,"case":case,"repetition":repetition,"condition":condition,
                            "attempt_id":attempt_id,"configuration_ref":configuration_ref,
                            "installation_ref":study.slots[index].installation_ref,"sample":sample})
                    }
                    Err(error) => {
                        unsafe_reuse = error.downcast_ref::<SandboxCleanupFailure>().is_some();
                        study.slots[index].state = "failed";
                        study.slots[index].error = Some(format!("{error:#}"));
                        json!({"schema_version":1,"case":case,"repetition":repetition,"condition":condition,
                            "attempt_id":attempt_id,"configuration_ref":configuration_ref,
                            "installation_ref":study.slots[index].installation_ref,"error":format!("{error:#}"),
                            "mutable_binding_reuse_safe":!unsafe_reuse})
                    }
                };
                let reference = seal(&measurement, &record, "compat-learning-condition-record")?;
                write_new(&root.join(format!("{}-{repetition}-{condition:?}.json", case.label())), &record)?;
                receipts.push(json!({"digest":reference.digest,"reference":reference,"condition":condition}));
                study.slots[index].record_ref = Some(reference);
                study.save(root)?;
                if unsafe_reuse {
                    acquired.node.lock().handle_unit_request(UnitRequest::FinishTrial {
                        attempt_id, outcome: TrialOutcome::Unknown, receipts })?;
                    anyhow::bail!("owned runtime retirement was not established; later fixture bindings remain unstarted");
                }
            }
            let complete = indices.iter().all(|&index| study.slots[index].state == "scored");
            let finished = acquired.node.lock().handle_unit_request(UnitRequest::FinishTrial {
                attempt_id, outcome: if complete { TrialOutcome::Succeeded } else { TrialOutcome::Unknown }, receipts })?;
            write_new(&root.join(format!("{}-{repetition}-trial-finished.json", case.label())), &finished)?;
            study.save(root)?;
        }
    }
    for (family, acquired) in &learned {
        let mut authority = acquired.node.lock();
        let fact = authority.installed_unit_fact(&acquired.candidate.identity.unit_id)?.context("candidate disappeared")?;
        ensure!(fact.revision == acquired.candidate.identity.revision
            && fact.artifact_digest == acquired.candidate.identity.artifact_digest && !fact.enabled
            && fact.descriptor.config.pointer("/cognition/adoption").is_none_or(Value::is_null),
            "measurement changed candidate identity, enabled state or adoption");
        let mut response = authority.handle_unit_request(UnitRequest::Export {
            unit_id: acquired.candidate.identity.unit_id.clone() })?;
        let export: UnitExport = serde_json::from_value(response.get_mut("export").context("measured export missing")?.take())?;
        let identity = world::export_identity(&export)?;
        ensure!(identity.unit_id == acquired.candidate.identity.unit_id
            && identity.revision == acquired.candidate.identity.revision
            && identity.artifact_digest == acquired.candidate.identity.artifact_digest,
            "post-trial export changed candidate identity");
        write_new(&root.join(format!("{}-candidate-after-trials.json", family.label())), &export)?;
        let observation = serde_json::to_vec(&json!({"schema_version":1,"measurement_only":true,"accepted":false,
            "family":family,"summary":study.summary,"acquisition_ref":acquired.acquisition_ref,
            "source_store_scope":"original development installation and its retained CAS"}))?;
        authority.handle_unit_request(UnitRequest::RegisterObservation {
            unit_id: acquired.candidate.identity.unit_id.clone(), revision: acquired.candidate.identity.revision.clone(),
            artifact_digest: acquired.candidate.identity.artifact_digest.clone(), kind: "compatibility_learning_measurement".into(),
            bytes: observation })?;
        // Keep the original acquisition store alive through export/finalization.
        let _source_store = &acquired.acquisition_store;
    }
    ensure!(study.acquisitions.iter().all(|acquisition| acquisition.state == "frozen")
        && study.slots.iter().all(|slot| slot.state == "scored"),
        "measurement retains failed or unstarted slots; see the original report and journals");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn compatibility_learning_is_measured_on_original_sources_and_independent_effects() -> Result<()> {
    if std::env::var("SHENKO_USE_LOCAL_MODEL").as_deref() != Ok("1") {
        eprintln!("SKIP learning_efficacy: set SHENKO_USE_LOCAL_MODEL=1, SHENKO_LOCAL_MODEL_URL and SHENKO_LOCAL_MODEL");
        return Ok(());
    }
    let base = std::env::var_os("SHENKO_SANDBOX_ROOT").map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/compat-learning-efficacy"));
    std::fs::create_dir_all(&base)?;
    let root = base.join(format!("run-{}", EntityId::new()));
    std::fs::create_dir(&root)?;
    let (broker, handle) = Broker::new()?;
    let mut study = Study::new(handle.binding().model());
    write_new(&root.join("planned-slots.json"), &study)?;
    std::fs::File::open(&root)?.sync_all()?;
    let broker_task = tokio::spawn(broker.run());
    let mut result = measure(&handle, &root, &mut study).await;
    let shutdown = match tokio::time::timeout(Duration::from_secs(120), handle.quiesce_for_handoff()).await {
        Ok(Ok(snapshot)) => json!(snapshot),
        Ok(Err(error)) => {
            let message = format!("original broker drain failed: {error:#}");
            result = Err(match result { Ok(()) => anyhow::anyhow!(message), Err(error) => error.context(message) });
            json!({"local_provider_readers_drained":false,"remote_termination_claimed":false,"error":format!("{error:#}")})
        }
        Err(_) => {
            result = Err(match result { Ok(()) => anyhow::anyhow!("original broker drain exceeded 120s"),
                Err(error) => error.context("original broker drain exceeded 120s") });
            json!({"local_provider_readers_drained":false,"remote_termination_claimed":false,"deadline_seconds":120})
        }
    };
    if let Err(error) = write_new(&root.join("broker-shutdown.json"), &shutdown) {
        result = Err(match result { Ok(()) => error, Err(primary) => primary.context(format!("record broker shutdown: {error:#}")) });
    }
    broker_task.abort();
    if let Err(error) = broker_task.await {
        if !error.is_cancelled() {
            result = Err(match result { Ok(()) => anyhow::anyhow!(error), Err(primary) => primary.context(format!("join broker: {error}")) });
        }
    }
    if let Err(error) = &result {
        study.fatal_error = Some(format!("{error:#}"));
        study.stop_unstarted("the original measurement stopped; no retry or replacement was issued");
    }
    study.save(&root)?;
    eprintln!("compatibility learning report: {}", root.join("report.json").display());
    result
}
