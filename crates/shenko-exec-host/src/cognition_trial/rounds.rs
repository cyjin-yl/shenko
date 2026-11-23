//! Repeated real-model turns in one real throwaway installation.
//!
//! This is the measurement surface for a local model that is expected to keep
//! working across several turns: the same Store, subject branch and provider
//! binding carry all rounds, the production subject loop performs each step,
//! the production dispatcher and native child consume delegated work, the
//! production provider client sends the request, and the production parser
//! turns the answer into a committed decision.
//!
//! What it never does: invent a provider, copy the prompt into the test, score
//! the answer with a keyword table, or treat an unstarted round as passed.
//! Every planned round is reported. A round whose window expired before it
//! began is `unstarted`, a round that started and did not produce a durable
//! decision is `failed`, and the exact model bytes, sealed input, decision and
//! owner observations are retained on disk for separate review.
//!
//! This runner records protocol and durability facts. It does not judge the
//! semantic quality of the model's answer; that remains a separate rubric
//! judgement on the retained artifacts.

use super::*;
use anyhow::{Context as _, anyhow, ensure};
use std::io::Write as _;

/// The bounded contract this runner implements. It is written into the plan
/// artifact so a report cannot be separated from what it claims to measure.
const ROUND_CONTRACT: &str = "shenko.cognition.real-model.rounds/v1: N sequential real subject turns in one throwaway installation; real Store, subject loop, dispatcher, provider wire and production parser; fixed pre-declared candidate binding and per-round protocol observations; verbatim artifacts retained; no synthetic provider, keyword scoring or pass-by-assertion";

/// The fixed identity of the candidate under observation.
///
/// All values are declared by the caller before the first inference and are
/// never derived from a model answer. An empty or absent field remains
/// visible as an unbound observation; it is not silently filled in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CandidateBinding {
    /// The exact source revision the caller claims these bytes come from.
    #[serde(default)]
    pub source_revision: String,
    /// A digest over `source_files` in the caller's declared order.
    #[serde(default)]
    pub source_digest: String,
    /// Path to the digest of each source file that the claims depend on.
    /// A missing file is recorded as `absent`, never skipped.
    #[serde(default)]
    pub source_files: BTreeMap<String, String>,
    /// The actual executable artifact that runs the harness (in this
    /// in-process consumer, the test binary containing the product code).
    #[serde(default)]
    pub executable_path: String,
    #[serde(default)]
    pub executable_sha256: String,
    #[serde(default)]
    pub executable_bytes: u64,
    /// Digest of the declared prompt components. It is not the complete
    /// per-step prompt (which also carries runtime and capability sections);
    /// the sealed per-round input plan is retained separately.
    #[serde(default)]
    pub declared_prompt_digest: String,
    /// The model identity the caller expects the configured provider binding
    /// to serve.
    #[serde(default)]
    pub model: String,
}

/// One planned turn in a repeated real-model run.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SandboxRoundSpec {
    /// A stable name used in reports and artifact file names.
    pub name: String,
    /// The external input to inject. `None` observes the subject's next idle
    /// continuation; it does not send a synthetic prompt.
    #[serde(default)]
    pub message: Option<String>,
}

impl SandboxRoundSpec {
    pub fn message(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self { name: name.into(), message: Some(message.into()) }
    }

    pub fn idle(name: impl Into<String>) -> Self {
        Self { name: name.into(), message: None }
    }
}

/// The kind of turn a round planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundKind {
    /// An external message was injected and must be consumed by this round.
    Message,
    /// No external input; the next committed subject continuation is the round.
    Idle,
}

/// The outcome of one fixed observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundObservationStatus {
    Passed,
    Failed,
    /// The observation does not apply to this round kind (for example a
    /// declared-message check on an idle round). It is never counted as pass.
    NotApplicable,
    /// The planned round never began because the run stopped first.
    Unstarted,
}

/// One fixed observation about a round. The name and meaning are fixed by the
/// runner; only the fact and its detail come from the run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundObservation {
    pub name: String,
    pub status: RoundObservationStatus,
    pub detail: String,
}

/// One retained file with its exact byte length and digest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxArtifact {
    /// Path relative to the report's `artifacts_root`.
    pub path: String,
    pub bytes: u64,
    /// Canonical `blake3:<hex>` digest of the retained bytes.
    pub digest: String,
}

/// The result of one planned turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxRoundRecord {
    pub index: usize,
    pub name: String,
    pub kind: RoundKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_digest: Option<String>,
    /// Whether this planned round actually began. A round whose window was
    /// already exhausted stays `false` and its observations are `unstarted`.
    pub started: bool,
    /// The actual equal share of the remaining run budget this round had.
    pub deadline_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<EntityId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_no: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_ref: Option<CommitRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_plan_ref: Option<ObjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_result_ref: Option<ObjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts_ref: Option<ObjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<ParsedOutput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output_digest: Option<String>,
    pub observations: Vec<RoundObservation>,
    /// Files retained for this round, relative to `artifacts_root`.
    pub artifacts: Vec<SandboxArtifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

/// The whole repeated-turn run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxRoundReport {
    pub schema_version: u32,
    pub contract: String,
    /// The fixed candidate declaration supplied before the run.
    pub candidate: CandidateBinding,
    /// Fixed candidate-level observations: source, executable, prompt and
    /// model identity. Unbound values fail; they are not filled from output.
    pub candidate_observations: Vec<RoundObservation>,
    pub model: String,
    /// Digest of the exact configured provider binding. The endpoint itself is
    /// not recorded; the digest identifies it without publishing the address.
    pub binding_digest: String,
    /// Non-secret environment facts needed to read the run.
    pub environment: BTreeMap<String, String>,
    pub started_at: String,
    pub finished_at: String,
    /// The whole-run window the caller supplied.
    pub deadline_ms: u64,
    pub planned_rounds: usize,
    pub started_rounds: usize,
    pub completed_rounds: usize,
    pub failed_rounds: usize,
    pub unstarted_rounds: usize,
    pub rounds: Vec<SandboxRoundRecord>,
    pub artifacts: Vec<SandboxArtifact>,
    pub artifacts_root: String,
    /// The primary run failure, if any. Per-round failures remain in their
    /// own records; this is setup, session or deadline failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// Cleanup that could not establish retirement. Unknown cleanup is a
    /// failed harness run, never a hidden success.
    pub cleanup_failures: Vec<String>,
    pub verdict: String,
    pub reason: String,
    pub boundaries: Vec<String>,
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn observation(name: &str, result: Result<()>, applicable: bool) -> RoundObservation {
    if !applicable {
        return RoundObservation {
            name: name.into(),
            status: RoundObservationStatus::NotApplicable,
            detail: "not applicable to this round kind".into(),
        };
    }
    match result {
        Ok(()) => RoundObservation {
            name: name.into(),
            status: RoundObservationStatus::Passed,
            detail: String::new(),
        },
        Err(error) => RoundObservation {
            name: name.into(),
            status: RoundObservationStatus::Failed,
            detail: format!("{error:#}"),
        },
    }
}

fn nonempty(label: &str, value: &str) -> Result<()> {
    ensure!(!value.trim().is_empty(), "{label} is not bound");
    Ok(())
}

fn sha256_hex(label: &str, value: &str) -> Result<()> {
    ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{label} is not a 64-character sha256 hex digest"
    );
    Ok(())
}

fn source_files_bound(files: &BTreeMap<String, String>) -> Result<()> {
    ensure!(!files.is_empty(), "no source files were declared");
    ensure!(
        files.values().all(|value| value != "absent" && !value.trim().is_empty()),
        "at least one declared source file is absent or undigested"
    );
    Ok(())
}

fn read_verified(store: &Store, reference: &ObjectRef) -> Result<Vec<u8>> {
    let bytes = store.cas.read_object(&reference.digest)?;
    ensure!(
        bytes.len() as u64 == reference.bytes && Digest::blake3(&bytes) == reference.digest,
        "object {} bytes do not match their reference",
        reference.digest.0
    );
    Ok(bytes)
}

fn artifact_stem(index: usize, name: &str) -> String {
    format!("round-{index:02}-{}", sanitize(name))
}

fn push_bytes_artifact(
    root: &Path,
    relative: &str,
    bytes: &[u8],
    artifacts: &mut Vec<SandboxArtifact>,
) -> Result<()> {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create artifact directory {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("create artifact {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("write artifact {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync artifact {}", path.display()))?;
    artifacts.push(SandboxArtifact {
        path: relative.to_string(),
        bytes: bytes.len() as u64,
        digest: Digest::blake3(bytes).0,
    });
    Ok(())
}

fn push_json_artifact(
    root: &Path,
    relative: &str,
    value: &impl Serialize,
    artifacts: &mut Vec<SandboxArtifact>,
) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    push_bytes_artifact(root, relative, &bytes, artifacts)
}

fn environment_snapshot() -> BTreeMap<String, String> {
    let mut environment = BTreeMap::new();
    environment.insert("os".into(), std::env::consts::OS.into());
    environment.insert("arch".into(), std::env::consts::ARCH.into());
    for key in [
        "SHENKO_USE_LOCAL_MODEL",
        "SHENKO_LOCAL_MODEL_REQUEST_TIMEOUT_MS",
        "SHENKO_LOCAL_INSECURE",
    ] {
        if let Ok(value) = std::env::var(key) {
            environment.insert(key.into(), value);
        }
    }
    environment
}

fn candidate_observations(candidate: &CandidateBinding, model: &str) -> Vec<RoundObservation> {
    vec![
        observation("candidate_source_revision", nonempty("source revision", &candidate.source_revision), true),
        observation("candidate_source_digest", nonempty("source digest", &candidate.source_digest), true),
        observation("candidate_source_files", source_files_bound(&candidate.source_files), true),
        observation("candidate_executable_sha256", sha256_hex("executable sha256", &candidate.executable_sha256), true),
        observation(
            "candidate_executable_bytes",
            (|| -> Result<()> {
                ensure!(candidate.executable_bytes > 0, "the executable artifact is empty");
                Ok(())
            })(),
            true,
        ),
        observation("candidate_declared_prompt_digest", nonempty("declared prompt digest", &candidate.declared_prompt_digest), true),
        observation(
            "candidate_model_binding",
            (|| -> Result<()> {
                ensure!(
                    candidate.model == model,
                    "declared model '{}' differs from the configured model '{}'",
                    candidate.model,
                    model
                );
                Ok(())
            })(),
            true,
        ),
    ]
}

fn round_verdict(
    failure: bool,
    cleanup_failed: bool,
    failed_rounds: usize,
    unstarted_rounds: usize,
    observation_failures: usize,
) -> &'static str {
    if failure || cleanup_failed || failed_rounds > 0 || observation_failures > 0 {
        "failed"
    } else if unstarted_rounds > 0 {
        "incomplete"
    } else {
        "complete"
    }
}

/// Run N sequential real-model turns in one real sandbox installation.
///
/// The caller owns the broker and must drain it after this returns. The
/// installation and every artifact under `artifacts_root` are retained; the
/// function refuses to overwrite an existing artifact file.
///
/// Each planned round gets an equal share of the remaining whole-run window,
/// so one stalled round cannot silently consume a later round's observation
/// time. The whole-run deadline remains the outer bound.
pub async fn run_sandbox_rounds<R: SandboxRuntime>(
    prepared: PreparedSandbox,
    broker: &BrokerHandle,
    catalog: Arc<CognitionCatalog>,
    rounds: &[SandboxRoundSpec],
    artifacts_root: &Path,
    candidate: CandidateBinding,
    stop: tokio::time::Instant,
    runtime: &R,
) -> Result<SandboxRoundReport> {
    ensure!(!rounds.is_empty(), "a repeated real-model run needs at least one round");
    ensure!(rounds.len() <= 64, "a repeated real-model run carries at most 64 rounds");

    let dir = prepared.installation().to_path_buf();
    let store = Arc::clone(prepared.store());
    let agent = prepared.agent_id();
    let (head, _) = store.load_subject(agent)?;
    let branch_id = head.branch_id.clone();
    let binding = broker.binding().clone();
    let model = binding.model().to_string();
    let binding_digest = Digest::blake3(&serde_json::to_vec(&binding)?).0;
    let environment = environment_snapshot();
    let started_at = chrono::Utc::now().to_rfc3339();
    let deadline_ms = millis(stop.saturating_duration_since(tokio::time::Instant::now()));
    // Round scheduling stops one bounded reserve before the caller's overall
    // deadline, so a completed last round still leaves real time to retire the
    // subject, dispatcher and supervisor instead of turning a clean run into
    // an ambiguous "shutdown exceeded the window".
    let initial_window = stop.saturating_duration_since(tokio::time::Instant::now());
    let cleanup_reserve = (initial_window / 10)
        .max(Duration::from_secs(1))
        .min(Duration::from_secs(30));
    let rounds_stop = stop.checked_sub(cleanup_reserve).unwrap_or(stop);
    std::fs::create_dir_all(artifacts_root)
        .with_context(|| format!("create artifacts root {}", artifacts_root.display()))?;

    let mut artifacts = Vec::new();
    let candidate_observations = candidate_observations(&candidate, &model);
    let planned_rounds: Vec<serde_json::Value> = rounds
        .iter()
        .enumerate()
        .map(|(index, round)| {
            serde_json::json!({
                "index": index,
                "name": &round.name,
                "kind": if round.message.is_some() { "message" } else { "idle" },
                "message_digest": round.message.as_ref().map(|message| Digest::blake3(message.as_bytes()).0),
            })
        })
        .collect();
    let plan = serde_json::json!({
        "schema_version": 1,
        "contract": ROUND_CONTRACT,
        "candidate": &candidate,
        "candidate_observations": &candidate_observations,
        "model": &model,
        "binding_digest": &binding_digest,
        "environment": &environment,
        "started_at": &started_at,
        "deadline_ms": deadline_ms,
        "rounds": &planned_rounds,
    });
    push_json_artifact(artifacts_root, "plan.json", &plan, &mut artifacts)?;

    let inbox = Arc::new(Inbox::new(8));
    let generation = RuntimeGeneration("real-model-rounds".into());
    let supervisor = Supervisor::new(generation.clone());
    let ctx = supervisor.context().clone();
    let subject_stopping = CancellationToken::new();
    let dispatcher_cancel = CancellationToken::new();
    let mut subject_task = None;
    let mut dispatcher_task = None;
    let mut identity_publication = None;
    let mut policy_publication = None;

    let mut records: Vec<SandboxRoundRecord> = rounds
        .iter()
        .enumerate()
        .map(|(index, spec)| SandboxRoundRecord {
            index,
            name: spec.name.clone(),
            kind: if spec.message.is_some() { RoundKind::Message } else { RoundKind::Idle },
            message_digest: spec.message.as_ref().map(|message| Digest::blake3(message.as_bytes()).0),
            started: false,
            deadline_ms: 0,
            started_at: None,
            finished_at: None,
            step_id: None,
            step_no: None,
            commit_ref: None,
            input_plan_ref: None,
            model_result_ref: None,
            facts_ref: None,
            output: None,
            raw_output_path: None,
            raw_output_bytes: None,
            raw_output_digest: None,
            observations: Vec::new(),
            artifacts: Vec::new(),
            failure: None,
        })
        .collect();
    let mut failure: Option<String> = None;
    let mut current_round: Option<usize> = None;

    let primary = tokio::time::timeout_at(rounds_stop, async {
        supervisor.publish_services().await?;
        identity_publication = Some(ctx
            .provide(Arc::new(IdentityService::new(Arc::new(DefaultIdentityPolicy))))
            .map_err(|error| anyhow!("identity service: {error}"))?);
        let (head, _) = store.load_subject(agent)?;
        policy_publication = Some(ctx
            .provide(Arc::new(PolicyRegistry::with_ledger(WindowLedger::new(Arc::new(
                StoreWindowLog::new(Arc::clone(&store), EntityId(head.branch_id.0)),
            )))))
            .map_err(|error| anyhow!("context policy: {error}"))?);
        supervisor.spawn(NativeChildPlugin, (Arc::clone(&store), generation)).await?;
        runtime.install(&supervisor, &prepared, &catalog).await?;
        let native = ctx.try_service::<NativeChildService>()
            .map_err(|error| anyhow!("native child service: {error}"))?;
        store.start_subject_dispatcher()?;
        let channel_config = NapCatConfig { bot_qq: 10000, ..Default::default() };
        let (channel_client, _events, _actions) = NapCatClient::open_persistent(
            channel_config.clone(), dir.join("napcat-ingress.db"))?;
        let state = SubjectLoopState::new(
            agent,
            Arc::clone(&inbox),
            Arc::clone(&store),
            ctx.clone(),
            qq_registry(channel_config, Arc::new(channel_client)),
        )?
        .with_broker(broker.clone())
        .with_hooks(Arc::clone(&supervisor.hooks))
        .with_awareness(Arc::clone(&catalog))
        .with_native_capabilities(native.installed_prompt_sections());
        let state = runtime.prepare_subject(&supervisor, &prepared, state).await?;
        subject_task = Some(tokio::spawn(run_subject_loop(state, subject_stopping.clone())));
        dispatcher_task = Some(tokio::spawn(run_committed_dispatcher(
            Arc::clone(&store),
            Arc::clone(&native),
            agent,
            Some(inbox.sender()),
            subject_stopping.clone(),
            dispatcher_cancel.clone(),
        )));

        let mut cursor: (u64, Option<EntityId>) = (0, None);
        let mut previous_step_no: Option<u64> = None;
        for (index, spec) in rounds.iter().enumerate() {
            current_round = Some(index);
            let now = tokio::time::Instant::now();
            if now >= rounds_stop {
                records[index].failure =
                    Some("the whole-run deadline was exhausted before this round began".into());
                continue;
            }
            let remaining_rounds = rounds.len() - index;
            let share = rounds_stop.saturating_duration_since(now) / (remaining_rounds as u32);
            let round_stop = rounds_stop.min(now + share);
            records[index].started = true;
            records[index].started_at = Some(chrono::Utc::now().to_rfc3339());
            records[index].deadline_ms = millis(round_stop.saturating_duration_since(now));

            let (input, message_event) = match &spec.message {
                Some(message) => {
                    let event = InboxEvent {
                        event_id: EntityId::new(),
                        kind: InboxEventKind::Message,
                        payload: serde_json::json!({
                            "post_type": "message",
                            "message_type": "private",
                            "user_id": 10001,
                            "sender": { "user_id": 10001, "nickname": "试用者" },
                            "raw_message": message,
                        }),
                        created_at: chrono::Utc::now(),
                        source: "napcat".into(),
                    };
                    inbox.sender().send(event.clone()).await
                        .map_err(|_| anyhow!("the sandbox inbox closed"))?;
                    (TrialObserverInput::Message(event.clone()), Some(event))
                }
                None => (TrialObserverInput::Idle, None),
            };
            let mut observer = TrialObserver {
                input,
                message_step_id: None,
                pending: BTreeMap::new(),
                consumed_results: Vec::new(),
                consumed_observations: Vec::new(),
            };
            let completed = tokio::time::timeout_at(round_stop, async {
                loop {
                    for (seq, step, decision) in
                        store.committed_decisions_after(agent, cursor.0, cursor.1, 64)?
                    {
                        cursor = (seq, Some(step.step_id));
                        let dispositions = decision.event_dispositions.clone();
                        if let Some(output) = observer.observe(
                            &store,
                            &catalog,
                            agent,
                            &dir,
                            &step,
                            decision,
                            runtime,
                        )? {
                            return Ok((step, output, dispositions));
                        }
                    }
                    ensure!(
                        !subject_task.as_ref().is_some_and(|task| task.is_finished()),
                        "sandbox subject terminated before the next round was observed"
                    );
                    ensure!(
                        !dispatcher_task.as_ref().is_some_and(|task| task.is_finished()),
                        "sandbox dispatcher terminated before the next round was observed"
                    );
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await;

            let (step, output, dispositions) = match completed {
                Ok(Ok(completed)) => completed,
                Ok(Err(error)) => {
                    records[index].failure = Some(format!("{error:#}"));
                    records[index].finished_at = Some(chrono::Utc::now().to_rfc3339());
                    failure = Some(format!("round {} failed: {error:#}", spec.name));
                    break;
                }
                Err(_) => {
                    let detail = format!(
                        "round {} exceeded its {} ms window without a committed decision",
                        spec.name, records[index].deadline_ms
                    );
                    records[index].failure = Some(detail.clone());
                    records[index].finished_at = Some(chrono::Utc::now().to_rfc3339());
                    failure = Some(detail);
                    break;
                }
            };

            records[index].step_id = Some(step.step_id);
            records[index].step_no = Some(step.step_no);
            records[index].commit_ref = step.commit_ref;
            records[index].input_plan_ref = Some(output.input_plan_ref.clone());
            records[index].model_result_ref = step.model_result_ref.clone();
            records[index].output = Some(output.output.clone());
            records[index].finished_at = Some(chrono::Utc::now().to_rfc3339());

            let measured: Result<()> = async {
                let stem = artifact_stem(index, &spec.name);
                let mut round_artifacts = Vec::new();

                let input_bytes = read_verified(&store, &output.input_plan_ref)?;
                push_bytes_artifact(
                    artifacts_root,
                    &format!("{stem}-input-plan.json"),
                    &input_bytes,
                    &mut round_artifacts,
                )?;

                let raw = step
                    .model_result_ref
                    .as_ref()
                    .context("the committed step retained no model result")?;
                let raw_bytes = read_verified(&store, raw)?;
                push_bytes_artifact(
                    artifacts_root,
                    &format!("{stem}-raw-output.txt"),
                    &raw_bytes,
                    &mut round_artifacts,
                )?;
                records[index].raw_output_path = Some(format!("{stem}-raw-output.txt"));
                records[index].raw_output_bytes = Some(raw_bytes.len() as u64);
                records[index].raw_output_digest = Some(Digest::blake3(&raw_bytes).0);

                if let Some(decision_ref) = &step.decision_ref {
                    let decision_bytes = read_verified(&store, decision_ref)?;
                    push_bytes_artifact(
                        artifacts_root,
                        &format!("{stem}-decision.json"),
                        &decision_bytes,
                        &mut round_artifacts,
                    )?;
                }

                let facts_ref = runtime.capture(&prepared, &step, &output).await?;
                if let Some(reference) = &facts_ref {
                    let facts_bytes = read_verified(&store, reference)?;
                    push_bytes_artifact(
                        artifacts_root,
                        &format!("{stem}-facts.json"),
                        &facts_bytes,
                        &mut round_artifacts,
                    )?;
                }
                records[index].facts_ref = facts_ref;

                let mut observations = Vec::new();
                observations.push(observation("committed_decision", Ok(()), true));
                observations.push(observation(
                    "round_identity",
                    (|| {
                        ensure!(
                            output.step_id == step.step_id,
                            "the observed decision is not this step's committed decision"
                        );
                        ensure!(
                            output.output.step_id == step.step_id,
                            "the parsed output belongs to another step"
                        );
                        ensure!(
                            step.branch_id == branch_id,
                            "the committed step belongs to another branch"
                        );
                        if let Some(previous) = previous_step_no {
                            ensure!(
                                step.step_no > previous,
                                "step number {} did not advance past the previous round's {}",
                                step.step_no,
                                previous
                            );
                        }
                        Ok(())
                    })(),
                    true,
                ));
                observations.push(observation("sealed_input_verified", Ok(()), true));
                observations.push(observation(
                    "model_result_retained",
                    Ok(()),
                    true,
                ));
                observations.push(observation(
                    "production_parser_raw_ref",
                    (|| -> Result<()> {
                        ensure!(
                            output.output.raw_ref == *raw,
                            "the parsed output does not reference the retained model result"
                        );
                        Ok(())
                    })(),
                    true,
                ));
                let declared_message_consumed = match message_event.as_ref() {
                    Some(message) => {
                        let disposition_ok = dispositions.iter().any(|disposition| {
                            disposition.event_id == message.event_id
                                && disposition.disposition != Disposition::Deferred
                        });
                        let commit = step
                            .commit_ref
                            .context("the message-consuming step has no commit")?;
                        ensure!(disposition_ok, "the declared message was not dispositioned");
                        ensure!(
                            store.event_disposition_commit(agent, message.event_id)? == Some(commit),
                            "the message disposition is not bound to this round's commit"
                        );
                        Ok(())
                    }
                    None => Ok(()),
                };
                observations.push(observation(
                    "declared_message_consumed",
                    declared_message_consumed,
                    message_event.is_some(),
                ));
                observations.push(observation(
                    "idle_continuation",
                    (|| -> Result<()> {
                        ensure!(
                            !output.output.text.trim().is_empty()
                                || !output.output.controls.is_empty(),
                            "the idle round produced neither prose nor an action"
                        );
                        Ok(())
                    })(),
                    message_event.is_none(),
                ));
                observations.push(observation(
                    "raw_output_retained",
                    (|| -> Result<()> {
                        ensure!(
                            records[index].raw_output_digest.is_some()
                                && records[index].raw_output_bytes.is_some_and(|bytes| bytes > 0),
                            "the raw model output was not retained"
                        );
                        Ok(())
                    })(),
                    true,
                ));

                records[index].observations = observations;
                records[index].artifacts = round_artifacts.clone();
                artifacts.extend(round_artifacts);
                push_json_artifact(
                    artifacts_root,
                    &format!("{stem}.json"),
                    &records[index],
                    &mut artifacts,
                )?;
                Ok(())
            }
            .await;

            match measured {
                Ok(()) => {
                    previous_step_no = Some(step.step_no);
                }
                Err(error) => {
                    records[index].failure = Some(format!("{error:#}"));
                    records[index].finished_at = Some(chrono::Utc::now().to_rfc3339());
                    if failure.is_none() {
                        failure = Some(format!("round {} evidence failed: {error:#}", spec.name));
                    }
                    // The session itself can still be healthy; keep the
                    // remaining planned rounds observable instead of
                    // pretending the run ended at the first artifact error.
                }
            }
        }
        Ok(())
    })
    .await;
    let primary = match primary {
        Ok(inner) => inner,
        Err(_) => Err(anyhow!("the repeated-model run exceeded its whole-run deadline")),
    };
    if let Err(error) = &primary {
        if failure.is_none() {
            failure = Some(format!("{error:#}"));
        }
        if let Some(index) = current_round {
            let record = &mut records[index];
            if record.started && record.step_id.is_none() && record.failure.is_none() {
                record.failure = Some(format!("{error:#}"));
                record.finished_at = Some(chrono::Utc::now().to_rfc3339());
            }
        }
    }

    for record in &mut records {
        if !record.started {
            if record.failure.is_none() {
                record.failure =
                    Some("not started: the run stopped before this planned round".into());
            }
            if record.observations.is_empty() {
                record.observations.push(RoundObservation {
                    name: "round_start".into(),
                    status: RoundObservationStatus::Unstarted,
                    detail: record.failure.clone().unwrap_or_default(),
                });
            }
        } else if record.step_id.is_none() && record.observations.is_empty() {
            record.observations.push(observation(
                "committed_decision",
                Err(anyhow!(
                    "{}",
                    record.failure.clone().unwrap_or_else(|| "round did not commit".into())
                )),
                true,
            ));
        } else if record.observations.is_empty() {
            record.observations.push(observation(
                "round_evidence",
                Err(anyhow!(
                    "{}",
                    record.failure.clone().unwrap_or_else(|| "round evidence was not recorded".into())
                )),
                true,
            ));
        }
    }

    let mut cleanup_failures = Vec::new();
    let mut execution_failures = Vec::new();
    subject_stopping.cancel();
    // Stop new plugin work even when the remaining window is too short to
    // confirm the hook/fiber teardown barrier. Unknown cleanup is reported.
    supervisor.lifecycle.shutdown();
    inbox.close().await;
    dispatcher_cancel.cancel();
    join_trial_task(
        "sandbox subject",
        &mut subject_task,
        &mut execution_failures,
        &mut cleanup_failures,
        stop,
    )
    .await;
    join_trial_task(
        "sandbox dispatcher",
        &mut dispatcher_task,
        &mut execution_failures,
        &mut cleanup_failures,
        stop,
    )
    .await;
    match tokio::time::timeout_at(stop, supervisor.stop()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => cleanup_failures.push(format!("sandbox supervisor shutdown: {error:#}")),
        Err(_) => cleanup_failures.push(
            "sandbox supervisor shutdown exceeded the run window; local cleanup is unconfirmed \
             and admitted provider requests retain their original receipts"
                .into(),
        ),
    }
    if !execution_failures.is_empty() {
        let detail = execution_failures.join("; ");
        if failure.is_none() {
            failure = Some(detail);
        }
    }

    let started_rounds = records.iter().filter(|record| record.started).count();
    let completed_rounds = records.iter().filter(|record| record.step_id.is_some()).count();
    let failed_rounds = records
        .iter()
        .filter(|record| record.started && record.step_id.is_none())
        .count();
    let unstarted_rounds = records.len().saturating_sub(started_rounds);
    let observation_failures: Vec<String> = records
        .iter()
        .flat_map(|record| {
            record
                .observations
                .iter()
                .filter(|observation| observation.status == RoundObservationStatus::Failed)
                .map(move |observation| {
                    format!("{}: {}: {}", record.name, observation.name, observation.detail)
                })
        })
        .collect();
    let candidate_failures: Vec<String> = candidate_observations
        .iter()
        .filter(|observation| observation.status == RoundObservationStatus::Failed)
        .map(|observation| format!("{}: {}", observation.name, observation.detail))
        .collect();
    let verdict = round_verdict(
        failure.is_some(),
        !cleanup_failures.is_empty(),
        failed_rounds,
        unstarted_rounds,
        observation_failures.len() + candidate_failures.len(),
    );
    let mut reason = format!(
        "planned {planned}, started {started_rounds}, completed {completed_rounds}, \
         failed {failed_rounds}, unstarted {unstarted_rounds}; \
         round observation failures {round_failures}, \
         candidate observation failures {candidate_failure_count}",
        planned = records.len(),
        round_failures = observation_failures.len(),
        candidate_failure_count = candidate_failures.len(),
    );
    if let Some(error) = &failure {
        reason.push_str(&format!("; primary failure: {error}"));
    }
    if !cleanup_failures.is_empty() {
        reason.push_str(&format!("; cleanup: {}", cleanup_failures.join("; ")));
    }
    if !observation_failures.is_empty() {
        reason.push_str(&format!("; round failures: {}", observation_failures.join("; ")));
    }
    if !candidate_failures.is_empty() {
        reason.push_str(&format!("; candidate failures: {}", candidate_failures.join("; ")));
    }

    let report = SandboxRoundReport {
        schema_version: 1,
        contract: ROUND_CONTRACT.into(),
        candidate,
        candidate_observations,
        model,
        binding_digest,
        environment,
        started_at,
        finished_at: chrono::Utc::now().to_rfc3339(),
        deadline_ms,
        planned_rounds: records.len(),
        started_rounds,
        completed_rounds,
        failed_rounds,
        unstarted_rounds,
        rounds: records,
        artifacts,
        artifacts_root: artifacts_root.display().to_string(),
        failure,
        cleanup_failures,
        verdict: verdict.into(),
        reason,
        boundaries: vec![
            "This harness runs the real Store, subject loop, dispatcher, provider wire and production parser in one in-process installation. It does not exercise the CLI, NapCat transport, packaging, restart or a multi-node topology.".into(),
            "Per-round observations cover durable identity, sealed input, retained model result, parser binding and round progress. Semantic quality, memory-recall correctness, awareness efficacy and 27B quality are not judged here; verbatim outputs are retained for separate rubric review.".into(),
            "The provider endpoint and credentials are not recorded; the binding digest identifies the configured provider without publishing a private address.".into(),
            "A completed round proves this candidate's behavior for these fixed turns on the configured model. It does not establish other scenarios, other models, long-running stability or 7/30-day observation.".into(),
        ],
    };
    let report_bytes = serde_json::to_vec_pretty(&report)?;
    let mut final_artifacts = report.artifacts.clone();
    push_bytes_artifact(artifacts_root, "report.json", &report_bytes, &mut final_artifacts)?;
    let manifest = final_artifacts
        .iter()
        .map(|artifact| format!("{}  {}", artifact.digest, artifact.path))
        .collect::<Vec<_>>()
        .join("\n");
    push_bytes_artifact(
        artifacts_root,
        "ARTIFACTS.blake3",
        format!("{manifest}\n").as_bytes(),
        &mut final_artifacts,
    )?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    // These cases are source-only in this packet and were not executed by the
    // preparer. The parent's verification run must execute them.
    use super::*;

    #[test]
    fn artifact_stems_are_bounded_and_distinct() {
        assert_eq!(artifact_stem(0, "hello world"), "round-00-hello_world");
        assert_eq!(artifact_stem(12, "a/b"), "round-12-a_b");
        assert_ne!(artifact_stem(1, "same"), artifact_stem(2, "same"));
    }

    #[test]
    fn unstarted_and_failed_rounds_never_become_complete() {
        assert_eq!(round_verdict(false, false, 0, 0, 0), "complete");
        assert_eq!(round_verdict(false, false, 0, 1, 0), "incomplete");
        assert_eq!(round_verdict(false, false, 1, 0, 0), "failed");
        assert_eq!(round_verdict(false, true, 0, 0, 0), "failed");
        assert_eq!(round_verdict(true, false, 0, 0, 0), "failed");
        assert_eq!(round_verdict(false, false, 0, 0, 1), "failed");
    }

    #[test]
    fn absent_source_files_are_visible_and_not_bound() {
        let mut files = BTreeMap::new();
        files.insert("a.rs".into(), "sha256:00".into());
        assert!(source_files_bound(&files).is_ok());
        files.insert("b.rs".into(), "absent".into());
        assert!(source_files_bound(&files).is_err());
    }

    #[test]
    fn candidate_observation_names_are_stable() {
        let candidate = CandidateBinding {
            source_revision: "abc".into(),
            source_digest: "sha256:00".into(),
            source_files: BTreeMap::from([("a.rs".into(), "sha256:00".into())]),
            executable_path: "/bin/true".into(),
            executable_sha256: "0".repeat(64),
            executable_bytes: 1,
            declared_prompt_digest: "sha256:00".into(),
            model: "m".into(),
        };
        let names: Vec<_> = candidate_observations(&candidate, "m")
            .into_iter()
            .map(|observation| observation.name)
            .collect();
        assert_eq!(
            names.join(","),
            "candidate_source_revision,candidate_source_digest,candidate_source_files,\
             candidate_executable_sha256,candidate_executable_bytes,\
             candidate_declared_prompt_digest,candidate_model_binding"
        );
    }
}
