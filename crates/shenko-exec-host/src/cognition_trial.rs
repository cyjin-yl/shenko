//! A finite sandbox comparison of one candidate cognition unit.
//!
//! This is the measurement path of SPEC §45.5 and §45.9: a unit is only
//! useful if it changes what the model actually decides, and the only honest
//! way to know is to run the candidate against the same installation with the
//! unit switched off and compare what the model did.
//!
//! What the comparison runs on:
//!
//! * a real throwaway installation — its own SQLite store, its own CAS, its
//!   own genesis, head and first step;
//! * the real subject loop (`run_subject_loop`), so the frozen input, the
//!   model request and the output parser are the production ones;
//! * the real dispatcher and a real native child service, so a model that
//!   asks for the unit body actually reads it and the answer reaches the
//!   next step;
//! * the real broker, so the trial's inference is metered like every other
//!   inference.
//!
//! What it never does: score the output with a keyword rule, compare against
//! a hard-coded expected answer, or let the caller state the outcome. The
//! judgement is a model judgement against a rubric the caller supplied, and
//! the judge never sees which condition produced the output.
//!
//! Every attempt is reported. A provider error, a judge error or an
//! exhausted budget is a failure of that attempt, it is counted, and it is
//! never quietly dropped to make a rate look better. When too little
//! survives, the verdict is `inconclusive` — not a pass.

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_awareness_core::{CognitionCatalog, parse_cognition_entry};
use shenko_broker::broker::BrokerHandle;
use shenko_broker::request::{
    InferenceMessage, InferencePayload, InferencePriority, InferenceRequest, InferenceStatus,
};
use shenko_channel_napcat::{NapCatClient, NapCatConfig, qq_registry};
use shenko_context_policy::{PolicyRegistry, StoreWindowLog, WindowLedger};
use shenko_identity::{DefaultIdentityPolicy, IdentityService};
use shenko_kernel::Supervisor;
use shenko_node::{Node, TrialOutcome};
use shenko_store::Store;
use shenko_subject::dispatch::run_committed_dispatcher;
use shenko_subject::inbox::{Inbox, InboxEvent, InboxEventKind};
use shenko_subject::loop_runner::{SubjectLoopState, run_subject_loop};
use shenko_subject::{
    NativeChildPlugin, NativeChildService, NativeTaskDescriptor, NativeTaskHandler,
    NativeTaskHandlerPlugin, NativeTaskInvocation,
};
use shenko_types::commit::CommitRef;
use shenko_types::evidence::{DecodeStatus, EvidenceReceipt};
use shenko_types::ids::*;
use shenko_types::object::{ArtifactRef, Digest, ObjectRef, SchemaRef, VersionRef};
use shenko_types::operation::*;
use shenko_types::output::{OutputAction, ParsedOutput};
use shenko_types::subject::*;
use shenko_types::task::{ImplementationRef, TaskRecord};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const TRIAL_CONTRACT: &[u8] = b"shenko.cognition.unit.trial/v1: finite sandbox comparison of one candidate unit against a disabled baseline; metered inference, no external effects";
const MAX_NORMALIZED_BYTES: usize = 480 * 1024;
/// The default wall-clock budget of one trial.
pub const DEFAULT_TRIAL_DEADLINE_MS: u64 = 120_000;
/// The largest wall-clock budget one trial may take.
pub const MAX_TRIAL_DEADLINE_MS: u64 = 900_000;
/// The largest number of trials per condition one trial may run.
pub const MAX_TRIALS_PER_CONDITION: usize = 8;
/// The largest number of scenarios one trial may carry.
pub const MAX_SCENARIOS: usize = 8;

/// One situation the unit is supposed to help with.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TrialScenario {
    /// A name for the scenario, used in the report.
    #[serde(default)]
    pub name: String,
    /// The message the subject receives.
    pub message: String,
    /// What "the unit helped" means, written as a rubric clause. It is given
    /// to the judge only; the subject never sees it.
    pub rubric: String,
}

/// What one trial measures.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TrialSpec {
    pub unit_id: String,
    pub revision: String,
    #[serde(default = "default_trials")]
    pub trials_per_condition: usize,
    #[serde(default = "default_deadline")]
    pub deadline_ms: u64,
    pub scenarios: Vec<TrialScenario>,
}

fn default_trials() -> usize {
    1
}

fn default_deadline() -> u64 {
    DEFAULT_TRIAL_DEADLINE_MS
}

/// One condition of a comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    /// The candidate unit is installed and offered.
    Candidate,
    /// The same installation with the candidate switched off.
    Control,
}

impl Condition {
    fn as_str(&self) -> &'static str {
        match self {
            Condition::Candidate => "candidate",
            Condition::Control => "control",
        }
    }
}

/// What one attempt produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrialSample {
    pub scenario: String,
    pub condition: Condition,
    pub trial: usize,
    /// `effective` when the judge found the behaviour the rubric asks for,
    /// `ineffective` when it did not, `failed` when the attempt itself did
    /// not complete.
    pub verdict: String,
    /// The judge's own words, kept verbatim.
    pub judge_text: String,
    /// Why the attempt failed, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// The digest of the sealed sample this attempt produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_digest: Option<String>,
    /// The subject's own words, bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_excerpt: Option<String>,
}

/// The totals of one condition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConditionTotals {
    /// How many attempts were started.
    pub attempted: usize,
    /// How many attempts produced a judge verdict.
    pub scored: usize,
    /// How many attempts failed before a verdict existed.
    pub failed: usize,
    /// How many scored attempts the judge found effective.
    pub effective: usize,
    /// The effective rate over the scored attempts. The attempted count is
    /// reported next to it: a rate over a smaller denominator never hides the
    /// attempts that did not make it.
    pub rate_over_scored: f64,
}

/// One scenario's comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioReport {
    pub name: String,
    pub message_digest: String,
    pub rubric_digest: String,
    pub candidate: ConditionTotals,
    pub control: ConditionTotals,
    /// The verdict of this scenario.
    pub verdict: String,
    /// Why the verdict is what it is.
    pub reason: String,
}

/// The whole measurement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrialReport {
    pub schema_version: u32,
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: String,
    /// The model the trial ran on. The endpoint is not stored: the binding's
    /// digest identifies it without publishing a private address.
    pub model: String,
    pub binding_digest: String,
    pub started_at: String,
    pub finished_at: String,
    pub trials_per_condition: usize,
    pub deadline_ms: u64,
    pub scenarios: Vec<ScenarioReport>,
    /// The verdict of the whole trial.
    pub verdict: String,
    pub reason: String,
    pub samples: Vec<TrialSample>,
}

/// The judge verdict on one output.
enum JudgeVerdict {
    Effective(String),
    Ineffective(String),
}

/// Judge one committed output against a rubric clause.
///
/// The judge is a model call against the same broker the trial used. It sees
/// the user message, the output and the rubric — never the installed unit
/// and never which condition produced the output.
async fn judge(
    broker: &BrokerHandle,
    rubric: &str,
    message: &str,
    output: &ParsedOutput,
) -> Result<JudgeVerdict> {
    let rendered = serde_json::json!({
        "reply_text": output.text,
        "controls": output.controls.iter().map(|control| &control.action).collect::<Vec<_>>(),
    });
    let (tx, rx) = tokio::sync::oneshot::channel();
    let request = InferenceRequest {
        request_id: EntityId::new(),
        agent_id: EntityId::new(),
        conversation_id: EntityId::new(),
        provider_binding: broker.binding().clone(),
        priority: InferencePriority::Normal,
        epoch: ControllerEpoch(1),
        payload: InferencePayload {
            system_prompt: format!(
                "你只做评审，不解决问题。判断标准：{rubric}\n\
                 只输出一行：EFFECTIVE 或 NOT_EFFECTIVE，然后给一句理由。\n\
                 如果这次输出直接下结论、没有体现核实，或者与本场景无关，判 NOT_EFFECTIVE。"
            ),
            messages: vec![InferenceMessage::text(
                "user",
                format!("用户消息：{message}\n模型这次输出：{rendered}"),
            )],
            max_tokens: Some(512),
            tools: Vec::new(),
            native_options: Default::default(),
        },
        result_tx: tx,
    };
    broker
        .submit(request)
        .map_err(|error| anyhow!("judge admission failed: {error}"))?;
    let result = rx
        .await
        .map_err(|_| anyhow!("judge produced no result"))?;
    ensure!(result.status == InferenceStatus::Success, "judge call failed: {:?}", result.status);
    let response = result.response.context("judge succeeded without a response")?;
    let text = response.content.trim().to_string();
    if text.is_empty() {
        bail!("judge returned an empty verdict");
    }
    if text.starts_with("EFFECTIVE") {
        Ok(JudgeVerdict::Effective(text))
    } else {
        Ok(JudgeVerdict::Ineffective(text))
    }
}

/// A fresh throwaway installation: store, genesis, head and first step.
fn sandbox(label: &str, root: &Path) -> Result<(PathBuf, Arc<Store>, EntityId)> {
    let dir = root.join(format!("{label}-{}", EntityId::new()));
    std::fs::create_dir_all(&dir)?;
    let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas"))?);
    let agent = EntityId::new();
    let first_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();
    let precommit = CommitRef::new(ledger_id, LedgerSeq(0));
    let object = store
        .cas
        .seal_object(b"sandbox", "text/plain", "sandbox", "raw", 1)?
        .object;
    let genesis = SubjectGenesis {
        genesis_id: EntityId::new(),
        subject_id: agent,
        bootstrap_id: EntityId::new(),
        launch_id: EntityId::new(),
        seed_manifest_ref: object.clone(),
        initial_profile_ref: object.clone(),
        home_manifest_ref: object.clone(),
        bootstrap_event_ref: object,
        created_commit: precommit,
    };
    let head = SubjectBranchHead {
        agent_id: agent,
        branch_id: BranchId::new(),
        subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration("cognition-trial".into()),
        genesis_ref: GenesisRef {
            ledger_id,
            genesis_id: genesis.genesis_id,
        },
        committed_step_no: 0,
        committed_step_id: None,
        committed_at: precommit,
        next_step_id: first_id,
        mode: SubjectMode::Active,
        mode_reason_ref: None,
        record_revision: RecordRevision(0),
    };
    let first = SubjectStep {
        step_id: first_id,
        branch_id: head.branch_id.clone(),
        step_no: 1,
        expected_parent_commit: precommit,
        controller_epoch: head.controller_epoch,
        software_generation: head.software_generation.clone(),
        state: SubjectStepState::Planned,
        input_plan_ref: None,
        context_revision: None,
        active_request_id: None,
        active_attempt_id: None,
        model_result_ref: None,
        model_outcome_ref: None,
        decision_ref: None,
        commit_ref: None,
        retry_reason_ref: None,
        record_revision: RecordRevision(0),
        created_by_step_id: None,
    };
    let root_task = TaskRecord::subject_root(
        head.subject_root_task_id, agent, genesis.seed_manifest_ref.clone(), precommit,
    );
    store.initialize_genesis(&genesis, &head, &first, &root_task)?;
    Ok((dir, store, agent))
}

/// Run one real subject step in a throwaway installation and return the
/// committed decision output.
///
/// The loop runs until a decision is committed. When that decision delegates
/// (the model asked to read the unit body, for instance), the dispatcher
/// runs the delegate and the answer arrives as the next step; the measured
/// output is then the one that could use it.
async fn committed_output(
    label: &str,
    root: &Path,
    broker: &BrokerHandle,
    catalog: Arc<CognitionCatalog>,
    message: &str,
    deadline: Duration,
) -> Result<ParsedOutput> {
    let (dir, store, agent) = sandbox(label, root)?;
    let channel_config = NapCatConfig { bot_qq: 10000, ..Default::default() };
    let (channel_client, _events, _actions) = NapCatClient::open_persistent(
        channel_config.clone(), dir.join("napcat-ingress.db"),
    )?;
    let inbox = Arc::new(Inbox::new(8));
    let generation = RuntimeGeneration("cognition-trial".into());
    let supervisor = Supervisor::new(generation.clone());
    let ctx = supervisor.context().clone();
    let _identity = ctx
        .provide(Arc::new(IdentityService::new(Arc::new(DefaultIdentityPolicy))))
        .map_err(|error| anyhow!("identity service: {error}"))?;
    let (head, _) = store.load_subject(agent)?;
    let _policy = ctx
        .provide(Arc::new(PolicyRegistry::with_ledger(WindowLedger::new(Arc::new(
            StoreWindowLog::new(Arc::clone(&store), EntityId(head.branch_id.0)),
        )))))
        .map_err(|error| anyhow!("context policy: {error}"))?;
    supervisor
        .spawn(NativeChildPlugin, (Arc::clone(&store), generation.clone()))
        .await?;
    let reader: Arc<dyn NativeTaskHandler> =
        Arc::new(crate::CognitionReadHandler::new(Arc::clone(&store), Arc::clone(&catalog)));
    supervisor.spawn(NativeTaskHandlerPlugin, reader).await?;
    let native = supervisor
        .context()
        .try_service::<NativeChildService>()
        .map_err(|error| anyhow!("native child service: {error}"))?;
    let state = SubjectLoopState::new(
        agent,
        Arc::clone(&inbox),
        Arc::clone(&store),
        ctx,
        qq_registry(channel_config, Arc::new(channel_client)),
    )?
    .with_broker(broker.clone())
    .with_awareness(catalog)
    .with_native_capabilities(native.installed_prompt_sections());
    let cancel = CancellationToken::new();
    let loop_task = tokio::spawn(run_subject_loop(state, cancel.clone()));
    let dispatcher = tokio::spawn(run_committed_dispatcher(
        Arc::clone(&store),
        Arc::clone(&native),
        agent,
        Some(inbox.sender()),
        cancel.clone(),
    ));
    inbox
        .sender()
        .send(InboxEvent {
            event_id: EntityId::new(),
            kind: InboxEventKind::Message,
            payload: serde_json::json!({
                "message_type": "private",
                "user_id": 10001,
                "sender": { "user_id": 10001, "nickname": "试用者" },
                "raw_message": message,
            }),
            created_at: chrono::Utc::now(),
            source: "napcat".into(),
        })
        .await
        .map_err(|_| anyhow!("the sandbox inbox closed"))?;
    let stop = tokio::time::Instant::now() + deadline;
    let committed = loop {
        let (head, _) = store.load_subject(agent)?;
        if let Some(committed) = head.committed_step_id {
            break committed;
        }
        if tokio::time::Instant::now() >= stop {
            cancel.cancel();
            bail!("the sandbox subject did not commit a step within its budget");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    // A decision that delegates is not the final answer: the delegate's
    // result arrives as the next step, and that step is the one the unit can
    // influence. Wait for it when the budget allows.
    let mut decided = committed;
    let branch = store.load_subject(agent)?.0.branch_id;
    if let Some(step) = store.get_step(&branch, committed)? {
        if let Some(reference) = step.decision_ref {
            let bytes = store.cas.read_object(&reference.digest)?;
            if let Ok(decision) = serde_json::from_slice::<StepDecision>(&bytes) {
                if !decision.delegated_task_intents.is_empty() {
                    decided = loop {
                        let (head, _) = store.load_subject(agent)?;
                        if let Some(step) = head.committed_step_id {
                            if step != committed {
                                break step;
                            }
                        }
                        if tokio::time::Instant::now() >= stop {
                            break committed;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    };
                }
            }
        }
    }
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(30), loop_task).await;
    let _ = tokio::time::timeout(Duration::from_secs(30), dispatcher).await;
    let step = store
        .get_step(&branch, decided)?
        .ok_or_else(|| anyhow!("the committed sandbox step is missing"))?;
    let reference = step
        .decision_ref
        .ok_or_else(|| anyhow!("the committed sandbox step has no decision"))?;
    let bytes = store.cas.read_object(&reference.digest)?;
    let decision: StepDecision = serde_json::from_slice(&bytes)?;
    Ok(decision.output)
}

/// The totals of one condition inside a list of samples.
fn totals_of(samples: &[TrialSample], condition: Condition) -> ConditionTotals {
    let mut totals = ConditionTotals::default();
    for sample in samples {
        if sample.condition != condition {
            continue;
        }
        totals.attempted += 1;
        match sample.verdict.as_str() {
            "effective" => {
                totals.scored += 1;
                totals.effective += 1;
            }
            "ineffective" => totals.scored += 1,
            _ => totals.failed += 1,
        }
    }
    totals.rate_over_scored = if totals.scored == 0 {
        0.0
    } else {
        totals.effective as f64 / totals.scored as f64
    };
    totals
}

/// The verdict of one comparison.
///
/// The verdict needs at least one scored attempt per condition. Without one
/// the trial is inconclusive: an unmeasured comparison is not a negative
/// result and not a positive one.
fn verdict_of(candidate: &ConditionTotals, control: &ConditionTotals) -> (&'static str, String) {
    if candidate.scored == 0 || control.scored == 0 {
        return (
            "inconclusive",
            format!(
                "the comparison has no scored attempt on one side (candidate scored {} of {}, control scored {} of {}); failed attempts: candidate {}, control {}",
                candidate.scored,
                candidate.attempted,
                control.scored,
                control.attempted,
                candidate.failed,
                control.failed
            ),
        );
    }
    let verdict = if candidate.rate_over_scored > control.rate_over_scored {
        "supported"
    } else {
        "not_supported"
    };
    (
        verdict,
        format!(
            "the candidate was effective in {} of {} scored attempts ({:.3}) against {} of {} ({:.3}) without it; attempted candidate {}, control {}",
            candidate.effective,
            candidate.scored,
            candidate.rate_over_scored,
            control.effective,
            control.scored,
            control.rate_over_scored,
            candidate.attempted,
            control.attempted
        ),
    )
}

/// Make one label safe for a directory name.
fn sanitize(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    if cleaned.is_empty() {
        "scenario".to_string()
    } else {
        cleaned
    }
}

/// Run the whole comparison.
///
/// The trial is bounded by the caller's budget. When the budget runs out the
/// report says so and lists what was attempted; nothing is silently
/// dropped.
pub async fn run_sandbox_trial(
    store: Arc<Store>,
    catalog: Arc<CognitionCatalog>,
    broker: BrokerHandle,
    sandbox_root: PathBuf,
    spec: &TrialSpec,
) -> Result<TrialReport> {
    let unit_id = spec.unit_id.trim().to_string();
    let revision = spec.revision.trim().to_string();
    ensure!(!unit_id.is_empty(), "the trial needs a unit id");
    ensure!(!revision.is_empty(), "the trial needs a revision");
    ensure!(
        spec.scenarios.len() <= MAX_SCENARIOS,
        "a trial carries at most {MAX_SCENARIOS} scenarios"
    );
    ensure!(!spec.scenarios.is_empty(), "a trial needs at least one scenario");
    let trials = spec.trials_per_condition.clamp(1, MAX_TRIALS_PER_CONDITION);
    let deadline_ms = spec.deadline_ms.clamp(1_000, MAX_TRIAL_DEADLINE_MS);
    let started_at = chrono::Utc::now().to_rfc3339();
    let stop = tokio::time::Instant::now() + Duration::from_millis(deadline_ms);

    // The candidate is the installed unit itself: its body is what the model
    // would read. The sandbox installs the same bytes, so the comparison is
    // about the unit and not about a re-typed copy of it.
    let installed = catalog.read(&unit_id, &revision)?;
    let entry = installed.entry_text.clone();
    let package = parse_cognition_entry(&entry, Path::new(&unit_id))?;
    let artifact_digest = installed.artifact_digest.clone();

    let binding = broker.binding().clone();
    let binding_digest = Digest::blake3(&serde_json::to_vec(&binding)?).0;
    let mut samples: Vec<TrialSample> = Vec::new();
    let mut scenarios: Vec<ScenarioReport> = Vec::new();

    for scenario in &spec.scenarios {
        let message_digest = Digest::blake3(scenario.message.as_bytes()).0;
        let rubric_digest = Digest::blake3(scenario.rubric.as_bytes()).0;
        let mut scenario_samples: Vec<TrialSample> = Vec::new();
        for trial in 0..trials {
            for condition in [Condition::Candidate, Condition::Control] {
                let remaining = stop.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    scenario_samples.push(TrialSample {
                        scenario: scenario.name.clone(),
                        condition,
                        trial,
                        verdict: "failed".to_string(),
                        judge_text: String::new(),
                        failure: Some("the trial budget was exhausted before this attempt".to_string()),
                        receipt_digest: None,
                        output_excerpt: None,
                    });
                    continue;
                }
                // The sandbox installation is the same in both conditions;
                // only the candidate's enable state differs.
                let root = sandbox_root.join(format!(
                    "{}-{trial}-{}",
                    sanitize(&scenario.name),
                    condition.as_str()
                ));
                std::fs::create_dir_all(&root)?;
                let node = Arc::new(parking_lot::Mutex::new(Node::open(&root.join("node"))?));
                let sandbox_catalog = Arc::new(CognitionCatalog::new(node));
                let mut candidate = package.clone();
                candidate.enabled = condition == Condition::Candidate;
                sandbox_catalog.install(&candidate, &root.join("cognition"))?;
                let label = format!("{}-{}", sanitize(&scenario.name), condition.as_str());
                match committed_output(
                    &label,
                    &root,
                    &broker,
                    Arc::clone(&sandbox_catalog),
                    &scenario.message,
                    remaining,
                )
                .await
                {
                    Ok(output) => {
                        let excerpt = output.text.chars().take(240).collect::<String>();
                        // Every attempt leaves a receipt in this installation's
                        // content store: the output the subject produced and
                        // what the judge said about it.
                        let receipt = store
                            .cas
                            .seal_object(
                                &serde_json::to_vec(&serde_json::json!({
                                    "schema_version": 1,
                                    "scenario": scenario.name,
                                    "condition": condition.as_str(),
                                    "trial": trial,
                                    "output": output.text,
                                    "controls": output.controls.len(),
                                }))?,
                                "application/json",
                                "native-cognition",
                                "cognition-trial-sample",
                                1,
                            )
                            .map(|sealed| sealed.object.digest.0.clone())
                            .unwrap_or_default();
                        match judge(&broker, &scenario.rubric, &scenario.message, &output).await {
                            Ok(JudgeVerdict::Effective(text)) => scenario_samples.push(TrialSample {
                                scenario: scenario.name.clone(),
                                condition,
                                trial,
                                verdict: "effective".to_string(),
                                judge_text: text,
                                failure: None,
                                receipt_digest: Some(receipt),
                                output_excerpt: Some(excerpt),
                            }),
                            Ok(JudgeVerdict::Ineffective(text)) => {
                                scenario_samples.push(TrialSample {
                                    scenario: scenario.name.clone(),
                                    condition,
                                    trial,
                                    verdict: "ineffective".to_string(),
                                    judge_text: text,
                                    failure: None,
                                    receipt_digest: Some(receipt),
                                    output_excerpt: Some(excerpt),
                                })
                            }
                            Err(error) => scenario_samples.push(TrialSample {
                                scenario: scenario.name.clone(),
                                condition,
                                trial,
                                verdict: "failed".to_string(),
                                judge_text: String::new(),
                                failure: Some(format!("{error:#}")),
                                receipt_digest: Some(receipt),
                                output_excerpt: Some(excerpt),
                            }),
                        }
                    }
                    Err(error) => scenario_samples.push(TrialSample {
                        scenario: scenario.name.clone(),
                        condition,
                        trial,
                        verdict: "failed".to_string(),
                        judge_text: String::new(),
                        failure: Some(format!("{error:#}")),
                        receipt_digest: None,
                        output_excerpt: None,
                    }),
                }
            }
        }
        let candidate_totals = totals_of(&scenario_samples, Condition::Candidate);
        let control_totals = totals_of(&scenario_samples, Condition::Control);
        let (verdict, reason) = verdict_of(&candidate_totals, &control_totals);
        scenarios.push(ScenarioReport {
            name: scenario.name.clone(),
            message_digest,
            rubric_digest,
            candidate: candidate_totals,
            control: control_totals,
            verdict: verdict.to_string(),
            reason: reason.to_string(),
        });
        samples.extend(scenario_samples);
    }

    let candidate_totals = totals_of(&samples, Condition::Candidate);
    let control_totals = totals_of(&samples, Condition::Control);
    let (verdict, reason) = verdict_of(&candidate_totals, &control_totals);
    Ok(TrialReport {
        schema_version: 1,
        unit_id,
        revision,
        artifact_digest,
        model: binding.model().to_string(),
        binding_digest,
        started_at,
        finished_at: chrono::Utc::now().to_rfc3339(),
        trials_per_condition: trials,
        deadline_ms,
        scenarios,
        verdict: verdict.to_string(),
        reason: reason.to_string(),
        samples,
    })
}

/// The native child that runs the comparison.
pub struct CognitionTrialHandler {
    store: Arc<Store>,
    catalog: Arc<CognitionCatalog>,
    broker: BrokerHandle,
    sandbox_root: PathBuf,
}

impl CognitionTrialHandler {
    pub fn new(
        store: Arc<Store>,
        catalog: Arc<CognitionCatalog>,
        broker: BrokerHandle,
        sandbox_root: PathBuf,
    ) -> Self {
        Self {
            store,
            catalog,
            broker,
            sandbox_root,
        }
    }

    fn plugin_id() -> EntityId {
        "8b41f0d6-2c95-4a37-9e64-70d5a1c8b3f2"
            .parse()
            .expect("fixed cognition-trial plugin UUID")
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self
            .store
            .cas
            .seal_object(bytes, "application/json", "native-cognition", schema, 1)
        {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => {
                self.store.close_external_writer();
                Err(error.into())
            }
        }
    }

    fn seal<T: Serialize + ?Sized>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        self.seal_bytes(&serde_json::to_vec(value)?, schema)
    }

    fn freeze(&self, value: serde_json::Value) -> Result<TrialSpec> {
        let spec: TrialSpec = serde_json::from_value(value)?;
        ensure!(
            spec.scenarios.len() <= MAX_SCENARIOS,
            "a trial carries at most {MAX_SCENARIOS} scenarios"
        );
        for scenario in &spec.scenarios {
            ensure!(
                !scenario.message.trim().is_empty(),
                "a scenario needs a message"
            );
            ensure!(
                !scenario.rubric.trim().is_empty(),
                "a scenario needs a rubric: without one there is nothing to judge"
            );
            ensure!(
                scenario.message.len() <= 4096 && scenario.rubric.len() <= 4096,
                "a scenario exceeds the size bound"
            );
        }
        Ok(spec)
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<TrialSpec> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "cognition_trial_frozen_input_mismatch"
        );
        let spec: TrialSpec = serde_json::from_slice(&bytes)?;
        Ok(spec)
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: "cognition.unit.trial.v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "measure_cognition_units".into(),
            extra: serde_json::json!({}),
        };
        let resource = self.seal(&bindings, "cognition-trial-binding")?;
        let acceptance = self.seal(
            "the measured comparison of the candidate against the disabled baseline, or an explicit failure",
            "cognition-trial-acceptance",
        )?;
        let retry = self.seal(
            "re-observe the recorded trial attempt under the original armed attempt; never run the comparison twice",
            "cognition-trial-retry",
        )?;
        let contract_digest = Digest::blake3(TRIAL_CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:cognition-unit-trial-v1".into(),
            },
            symbol: "cognition.unit.trial.execute".into(),
            schema: SchemaRef {
                name: "cognition-trial-observation-v1".into(),
                major: 1,
                digest: contract_digest.clone(),
            },
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let operation = OperationRecord {
            record_schema_version: 1,
            operation_id: invocation.operation_id,
            record_revision: RecordRevision(0),
            task_id: invocation.task_invocation.task_id,
            logical_key: "native-handler".into(),
            operation_name: "cognition.unit.trial.v1".into(),
            input_schema_version: 1,
            output_schema_version: 1,
            input_digest: input_ref.digest.clone(),
            final_input_ref: input_ref,
            contract_ref: OperationContractRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: contract_digest.clone(),
                contract_digest,
                receipt_reader_ref: version.clone(),
            },
            resource_bindings_ref: resource,
            authority_ref: invocation.authority.clone(),
            acceptance_predicate_ref: acceptance,
            retry_contract_ref: retry,
            checkpoint_scope_ref: None,
            state: OperationState::InFlight,
            outcome: None,
            outcome_conflict: false,
            effect_status: EffectStatus::None,
            retry_of: None,
            compensates: None,
            possible_duplicate_of: vec![],
            prepared_commit: zero,
            evidence_through: None,
        };
        let attempt = AttemptRecord {
            record_schema_version: 1,
            attempt_id: EntityId::new(),
            operation_id: invocation.operation_id,
            attempt_no: 1,
            invocation_id: invocation.task_invocation.invocation_id,
            worker_id: invocation.task_invocation.worker_id,
            node_id: invocation.task_invocation.node_id,
            epoch: invocation.task_invocation.epoch,
            implementation_ref: version,
            dispatch_stage: AttemptDispatchStage::Armed,
            armed_commit: zero,
            provider_handle: None,
            provider_idempotency: None,
            transport_ref: None,
            started_at: None,
            last_observed_at: None,
            final_receipt_id: None,
        };
        Ok((operation, attempt))
    }

    /// The plan the attempt is bound to.
    ///
    /// It names the exact artifact, the scenarios and their rubrics, the
    /// candidate and the control configuration and the model binding, so a
    /// later reader can see exactly what was measured.
    fn plan(&self, spec: &TrialSpec, unit_id: &str, revision: &str, artifact: &str) -> serde_json::Value {
        serde_json::json!({
            "unit_id": unit_id,
            "revision": revision,
            "artifact_digest": artifact,
            "scenarios": spec.scenarios.iter().map(|scenario| serde_json::json!({
                "name": scenario.name,
                "message_digest": Digest::blake3(scenario.message.as_bytes()).0,
                "rubric_digest": Digest::blake3(scenario.rubric.as_bytes()).0,
            })).collect::<Vec<_>>(),
            "candidate": {"enabled": true, "unit_id": unit_id, "revision": revision},
            "control": {"enabled": false, "unit_id": unit_id, "revision": revision},
            "model_binding_digest": Digest::blake3(&serde_json::to_vec(self.broker.binding()).unwrap_or_default()).0,
            "model": self.broker.binding().model(),
            "trials_per_condition": spec.trials_per_condition,
            "deadline_ms": spec.deadline_ms,
        })
    }

    /// Run the comparison and record it in the installation authority.
    async fn measure(&self, spec: &TrialSpec) -> Result<serde_json::Value> {
        let unit = self
            .catalog
            .units()?
            .into_iter()
            .find(|unit| unit.unit_id == spec.unit_id.trim())
            .ok_or_else(|| anyhow!("the unit '{}' is not installed here", spec.unit_id))?;
        let artifact = Digest(unit.artifact_digest.clone());
        // The attempt is opened before anything runs and closed with the
        // receipts of the run, so the measurement is owned by the runner and
        // not by the caller's description of it.
        let plan = self.plan(spec, &unit.unit_id, &unit.revision, &unit.artifact_digest);
        let opened = self.catalog.begin_trial(
            &unit.unit_id,
            &unit.revision,
            &artifact,
            "sandbox_comparison",
            &plan,
        )?;
        let attempt_id = opened
            .get("attempt_id")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        let report = run_sandbox_trial(
            Arc::clone(&self.store),
            Arc::clone(&self.catalog),
            self.broker.clone(),
            self.sandbox_root.clone(),
            spec,
        )
        .await;
        let report = match report {
            Ok(report) => report,
            Err(error) => {
                // The attempt never produced a measurement: it is closed as
                // unknown, never as a result.
                let _ = self
                    .catalog
                    .finish_trial(&attempt_id, TrialOutcome::Unknown, &[]);
                return Err(error);
            }
        };
        let bytes = serde_json::to_vec(&report)?;
        let receipts: Vec<serde_json::Value> = report
            .samples
            .iter()
            .filter_map(|sample| {
                let digest = sample.receipt_digest.clone()?;
                Some(serde_json::json!({
                    "kind": "sample",
                    "scenario": sample.scenario,
                    "condition": sample.condition,
                    "trial": sample.trial,
                    "verdict": sample.verdict,
                    "digest": digest,
                }))
            })
            .collect();
        let outcome = match report.verdict.as_str() {
            "supported" => TrialOutcome::Succeeded,
            "not_supported" => TrialOutcome::Failed,
            _ => TrialOutcome::Unknown,
        };
        let finished = self
            .catalog
            .finish_trial(&attempt_id, outcome, &receipts)?;
        // The report itself is an observation: bytes this installation holds
        // and can re-verify. It never becomes acceptance by itself.
        let observation = self.catalog.register_observation(
            &unit.unit_id,
            &unit.revision,
            &artifact,
            "sandbox_comparison",
            &bytes,
        )?;
        Ok(serde_json::json!({
            "schema_version": 1,
            "status": "ok",
            "op": "trial",
            "attempt_id": attempt_id,
            "trial": finished,
            "observation": observation,
            "report": report,
        }))
    }

    /// Record the outcome of one trial run.
    fn record(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        result: serde_json::Value,
        outcome: OperationOutcome,
    ) -> Result<OperationRecord> {
        let bytes = serde_json::to_vec(&result)?;
        let raw = self.seal_bytes(&bytes, "cognition-trial-observation")?;
        let normalized = if bytes.len() <= MAX_NORMALIZED_BYTES {
            raw.clone()
        } else {
            self.seal(
                &serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "error": {
                        "code": "normalized_result_too_large",
                        "message": "the exact observation is sealed in raw evidence"
                    }
                }),
                "cognition-trial-result",
            )?
        };
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "cognition.unit.trial.v1".into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("cognition-trial:{}", attempt.attempt_id)),
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: None,
            normalized_ref: Some(normalized),
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.store.record_external_evidence(
            operation.operation_id,
            attempt.attempt_id,
            evidence,
            Some(outcome),
            EffectStatus::Complete,
        )
    }

    async fn run(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(
            invocation.task.kind == "cognition.unit.trial"
                && invocation.task.definition_version == 1,
            "cognition_trial_task_identity_mismatch"
        );
        let frozen = self.freeze(std::mem::take(&mut invocation.input))?;
        let input_ref = self.seal(&frozen, "cognition-trial-frozen-input")?;
        let (mut operation, mut attempt) = self.operation_parts(&invocation, input_ref)?;
        if let Some(existing) = self.store.arm_external_operation(
            &invocation.task_invocation,
            &mut operation,
            &mut attempt,
        )? {
            return Ok(existing);
        }
        let (result, outcome) = match self.measure(&frozen).await {
            Ok(result) => (result, OperationOutcome::Succeeded),
            Err(error) => (
                serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "op": "trial",
                    "error": {"code": "trial_failed", "message": format!("{error:#}")},
                }),
                OperationOutcome::Failed,
            ),
        };
        self.record(&operation, &attempt, result, outcome)
    }
}

#[async_trait]
impl NativeTaskHandler for CognitionTrialHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "cognition.unit.trial".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(TRIAL_CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-cognition-trial/1".into(),
            authority_refs: vec!["cognition_unit.trial/v1".into()],
            prompt_section: Some(
                "cognition.unit.trial/v1 在真实沙箱里把某个单元和“关掉它”的同一安装做一次有限对照：[delegate:cognition.unit.trial:1] {\"unit_id\":\"...\",\"revision\":\"...\",\"scenarios\":[{\"name\":\"...\",\"message\":\"...\",\"rubric\":\"...\"}],\"trials_per_condition\":1,\"deadline_ms\":180000}。rubric 只给评审看，描述“这次输出体现了单元应有的判断”的判据；消息是你要试的真实场景。对照组是同一安装关闭该单元，评审看不到哪一边。失败、超时、评审出错都会被如实计入分母，样本不足时结论是 inconclusive 而不是通过。结论只是本机这一次的测量，不是这个单元永远有效的证明。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id
                && operation.operation_name == "cognition.unit.trial.v1",
            "original_operation_contract_mismatch"
        );
        if operation.state == OperationState::Resolved {
            return Ok(operation);
        }
        let attempt = self.store.external_operation_attempt(operation_id)?;
        if operation.state != OperationState::InFlight
            || attempt.dispatch_stage != AttemptDispatchStage::Armed
        {
            return Ok(operation);
        }
        // The comparison already ran. Reconciling re-reads what it recorded;
        // it never runs the comparison a second time.
        let frozen = self.frozen(&operation)?;
        let observations = self.catalog.observations(&frozen.unit_id)?;
        let trials = self.catalog.trial_evidence(&frozen.unit_id, "0", 16)?;
        let result = serde_json::json!({
            "schema_version": 1,
            "status": "ok",
            "op": "trial",
            "reconciled": true,
            "unit_id": frozen.unit_id,
            "revision": frozen.revision,
            "observations": observations,
            "trials": trials,
        });
        self.record(&operation, &attempt, result, OperationOutcome::Succeeded)
    }
}
