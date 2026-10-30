//! Awareness efficacy in a real Shenko sandbox.
//!
//! An installable awareness unit is only useful if it changes what the local
//! model actually decides. This test therefore drives the *real* subject loop —
//! real Store, real inbox batch, real S0 projection, real provider wire, real
//! output parser, real S1 commit — in a throwaway sandbox, twice per scenario:
//! once with the unit enabled and once with it disabled. It then measures how
//! often the model takes the bounded action the unit is meant to suggest.
//!
//! Baseline intelligence is the deployed local model (Qwen 3.8 27B NVFP4 class);
//! the endpoint and model come from the environment, so the harness is not tied
//! to one model id:
//!
//! ```sh
//! SHENKO_USE_LOCAL_MODEL=1 \
//! SHENKO_LOCAL_MODEL_URL=http://127.0.0.1:8000 SHENKO_LOCAL_MODEL=<served id> \
//! SHENKO_AWARENESS_TRIALS=6 SHENKO_AWARENESS_MIN_RATE=0.7 \
//! cargo test -p shenko-subject --test awareness_efficacy -- --nocapture
//! ```
//!
//! Sandboxes under `SHENKO_SANDBOX_ROOT` (default `build/awareness-efficacy`)
//! keep the real commits, decisions and CAS objects as evidence.

use shenko_awareness_core::AwarenessRegistry;
use shenko_broker::broker::{Broker, BrokerHandle};
use shenko_broker::local_provider::LocalModelProvider;
use shenko_broker::request::{
    InferenceMessage, InferencePayload, InferencePriority, InferenceRequest,
};
use shenko_store::store::Store;
use shenko_subject::inbox::{Inbox, InboxEvent, InboxEventKind};
use shenko_subject::loop_runner::{SubjectLoopState, run_subject_loop};
use shenko_types::commit::CommitRef;
use shenko_types::ids::*;
use shenko_types::ids::ControllerEpoch;
use shenko_types::output::{OutputAction, ParsedOutput};
use shenko_types::subject::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tokio::sync::oneshot;

/// One behavior a unit is supposed to make more likely.
struct Scenario {
    name: &'static str,
    unit_id: &'static str,
    message: &'static str,
    /// What "effective" means for this scenario, written as a rubric clause.
    /// It is given only to the judge, never to the subject.
    rubric: &'static str,
    /// A unit that has not yet met the bar stays `Tentative`: it is still
    /// measured and reported, but the run does not pretend it works.
    accepted: bool,
}

/// Judge one committed output against a rubric clause.
///
/// The verdict is a model judgement, not a keyword scan: a list of "check
/// words" would be exactly the hard-coded detection this design rejects, and it
/// would score a model that merely echoes a phrase. The judge runs on the same
/// local baseline by default (`SHENKO_JUDGE_MODEL_URL` / `SHENKO_JUDGE_MODEL`
/// override it) and sees only the user message, the model output and the
/// rubric — never the installed unit.
enum Verdict {
    Effective,
    Ineffective(String),
}

/// Judge one committed output against a rubric clause.
///
/// Returns `Err` only when the judge itself failed (timeout, provider error, or
/// no verdict after one retry). That is deliberately separate from an
/// `Ineffective` verdict so a broken judge cannot silently depress the measured
/// rate in either direction.
async fn judge(
    judge: &LocalModelProvider,
    rubric: &str,
    message: &str,
    output: &ParsedOutput,
) -> Result<Verdict, String> {
    let rendered = serde_json::json!({
        "reply_text": output.text,
        "controls": output.controls.iter().map(|control| match &control.action {
            OutputAction::Delegate { text, .. } => format!("delegate: {text}"),
            OutputAction::DelegateModel { spec } => format!("delegate: {}", spec.goal),
            OutputAction::Remember { text } => format!("remember: {text}"),
            OutputAction::Reply { target, .. } => format!("reply to {target:?}"),
            OutputAction::Identity { .. } => "identity change".to_string(),
            OutputAction::SemanticWait { change } => format!("semantic wait: {change:?}"),
        }).collect::<Vec<_>>(),
    });
    let (result_tx, _result_rx) = oneshot::channel();
    let request = InferenceRequest {
        request_id: EntityId::new(),
        agent_id: EntityId::new(),
        conversation_id: EntityId::new(),
        provider_binding: judge.binding(),
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
        result_tx,
    };
    for attempt in 0..3 {
        let response = tokio::time::timeout(Duration::from_secs(120), judge.complete(&request))
            .await
            .map_err(|_| "judge timed out".to_string())?
            .map_err(|error| format!("judge call failed: {error}"))?;
        let verdict = response.content.trim();
        if verdict.is_empty() {
            if attempt < 2 {
                continue;
            }
            return Err("judge returned no verdict after three attempts".to_string());
        }
        return Ok(if verdict.starts_with("EFFECTIVE") {
            Verdict::Effective
        } else {
            Verdict::Ineffective(format!(
                "{verdict}: {}",
                output.text.chars().take(240).collect::<String>()
            ))
        });
    }
    Err("judge returned no verdict".to_string())
}

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "network_change_is_checked_not_assumed",
            unit_id: "awareness.network-change",
            message: "刚才还能上网，现在什么都打不开了，你说这是怎么回事",
            rubric: "模型是否把“自己此刻仍然在线”当成需要一次真实观察才能成立的假设，而不是直接给出原因或结论？",
            accepted: false,
        },
        Scenario {
            name: "capability_gap_is_verified_before_claiming",
            unit_id: "awareness.capability-gap",
            message: "你能帮我把这段录音转成字幕吗",
            rubric: "模型是否以“这次的实际依据”回答能力问题（例如先取得一次真实证据、或明确说明尚未验证），而不是只凭印象断言能做或不能做？",
            accepted: false,
        },
    ]
}

fn sandbox_root() -> PathBuf {
    std::env::var("SHENKO_SANDBOX_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/awareness-efficacy")
        })
}

/// A fresh real installation: ledger, genesis and the first planned step.
fn sandbox(label: &str) -> (PathBuf, Arc<Store>, EntityId) {
    let dir = sandbox_root().join(format!("{label}-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap());
    let agent = EntityId::new();
    let first_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();
    let precommit = CommitRef::new(ledger_id, LedgerSeq(0));
    let object = store
        .cas
        .seal_object(b"sandbox", "text/plain", "sandbox", "raw", 1)
        .unwrap()
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
        software_generation: RuntimeGeneration("awareness-efficacy".into()),
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
    store.initialize_genesis(&genesis, &head, &first).unwrap();
    (dir, store, agent)
}

/// Run one real subject step through the production loop and return the
/// committed decision output.
async fn committed_output(
    label: &str,
    handle: &BrokerHandle,
    awareness: Arc<AwarenessRegistry>,
    message: &str,
) -> Result<ParsedOutput, String> {
    let (_dir, store, agent) = sandbox(label);
    let inbox = Arc::new(Inbox::new(8));
    let ctx = cordis_core::Context::new();
    let _identity = ctx
        .provide(Arc::new(shenko_identity::IdentityService::new(Arc::new(
            shenko_identity::DefaultIdentityPolicy,
        ))))
        .map_err(|error| format!("identity service: {error}"))?;
    let (head, _) = store.load_subject(agent).map_err(|e| e.to_string())?;
    let _context = ctx
        .provide(Arc::new(shenko_context_policy::PolicyRegistry::with_ledger(
            shenko_context_policy::WindowLedger::new(Arc::new(
                shenko_context_policy::StoreWindowLog::new(
                    Arc::clone(&store),
                    EntityId(head.branch_id.0),
                ),
            )),
        )))
        .map_err(|error| format!("context policy: {error}"))?;
    let state = SubjectLoopState::new(agent, Arc::clone(&inbox), store.clone(), ctx)
        .map_err(|e| e.to_string())?
        .with_broker(handle.clone())
        .with_awareness(awareness);
    let cancel = CancellationToken::new();
    let loop_task = tokio::spawn(run_subject_loop(state, cancel.clone()));
    inbox
        .sender()
        .send(InboxEvent {
            event_id: EntityId::new(),
            kind: InboxEventKind::Message,
            payload: serde_json::json!({
                "message_type": "private",
                "user_id": 10001,
                "sender": { "user_id": 10001, "nickname": "阿岚" },
                "raw_message": message,
            }),
            created_at: chrono::Utc::now(),
            source: "napcat".into(),
        })
        .await
        .map_err(|_| "subject inbox closed".to_string())?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    let committed = loop {
        let (head, _) = store.load_subject(agent).map_err(|e| e.to_string())?;
        if let Some(committed) = head.committed_step_id {
            break committed;
        }
        if tokio::time::Instant::now() >= deadline {
            cancel.cancel();
            return Err("the subject did not commit a step within 180s".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(30), loop_task).await;
    let step = store
        .get_step(
            &store.load_subject(agent).map_err(|e| e.to_string())?.0.branch_id,
            committed,
        )
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "committed step missing".to_string())?;
    let object = step
        .decision_ref
        .ok_or_else(|| "committed step has no decision".to_string())?;
    let bytes = store
        .cas
        .read_object(&object.digest)
        .map_err(|e| e.to_string())?;
    let decision: StepDecision = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    Ok(decision.output)
}

#[tokio::test(flavor = "current_thread")]
async fn awareness_units_earn_their_place_in_the_sandbox() {
    if std::env::var("SHENKO_USE_LOCAL_MODEL").as_deref() != Ok("1") {
        eprintln!(
            "SKIP awareness_efficacy: set SHENKO_USE_LOCAL_MODEL=1, SHENKO_LOCAL_MODEL_URL and \
             SHENKO_LOCAL_MODEL to measure a unit against the real local model"
        );
        return;
    }
    let model = std::env::var("SHENKO_LOCAL_MODEL").unwrap_or_else(|_| "unset".into());
    let trials: usize = std::env::var("SHENKO_AWARENESS_TRIALS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(6)
        .clamp(1, 16);
    let min_rate: f64 = std::env::var("SHENKO_AWARENESS_MIN_RATE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.7)
        .clamp(0.0, 1.0);

    let (broker, handle) = Broker::new().expect("local provider from the environment");
    let broker_task = tokio::spawn(broker.run());
    let judge_provider = LocalModelProvider::new(
        std::env::var("SHENKO_JUDGE_MODEL_URL")
            .or_else(|_| std::env::var("SHENKO_LOCAL_MODEL_URL"))
            .expect("judge endpoint"),
        std::env::var("SHENKO_JUDGE_MODEL")
            .or_else(|_| std::env::var("SHENKO_LOCAL_MODEL"))
            .expect("judge model"),
    );

    let mut failures: Vec<String> = Vec::new();
    let mut summary: Vec<serde_json::Value> = Vec::new();
    for scenario in scenarios() {
        let mut enabled_ok = 0_usize;
        let mut disabled_ok = 0_usize;
        let mut enabled_failures: Vec<String> = Vec::new();
        let mut disabled_failures: Vec<String> = Vec::new();
        let mut judge_errors: Vec<String> = Vec::new();
        let mut observed: Vec<serde_json::Value> = Vec::new();
        for trial in 0..trials {
            let registry = Arc::new(AwarenessRegistry::with_seeds());
            registry.enable(scenario.unit_id).unwrap();
            let label = format!("{}-{trial}-on", scenario.name);
            match committed_output(&label, &handle, Arc::clone(&registry), scenario.message).await {
                Ok(output) => match judge(&judge_provider, scenario.rubric, scenario.message, &output).await {
                    Ok(verdict) => {
                        let effective = matches!(verdict, Verdict::Effective);
                        if effective {
                            enabled_ok += 1;
                        } else if let Verdict::Ineffective(failure) = verdict {
                            enabled_failures.push(failure);
                        }
                        observed.push(serde_json::json!({
                            "condition": "with_unit",
                            "effective": effective,
                            "text": output.text.chars().take(160).collect::<String>(),
                        }));
                    }
                    Err(error) => judge_errors.push(format!("with unit: {error}")),
                },
                Err(error) => enabled_failures.push(error),
            }
            registry.disable(scenario.unit_id).unwrap();
            let label = format!("{}-{trial}-off", scenario.name);
            match committed_output(&label, &handle, Arc::clone(&registry), scenario.message).await {
                Ok(output) => match judge(&judge_provider, scenario.rubric, scenario.message, &output).await {
                    Ok(verdict) => {
                        let effective = matches!(verdict, Verdict::Effective);
                        if effective {
                            disabled_ok += 1;
                        } else if let Verdict::Ineffective(failure) = verdict {
                            disabled_failures.push(failure);
                        }
                        observed.push(serde_json::json!({
                            "condition": "without_unit",
                            "effective": effective,
                            "text": output.text.chars().take(160).collect::<String>(),
                        }));
                    }
                    Err(error) => judge_errors.push(format!("without unit: {error}")),
                },
                Err(error) => disabled_failures.push(error),
            }
        }
        // Judge failures are excluded from the denominator instead of counted as
        // model failures; a run with too many of them is not trustworthy.
        let enabled_valid = trials.saturating_sub(
            judge_errors.iter().filter(|error| error.starts_with("with unit")).count(),
        );
        let disabled_valid = trials.saturating_sub(
            judge_errors.iter().filter(|error| error.starts_with("without unit")).count(),
        );
        let enabled_rate = if enabled_valid == 0 { 0.0 } else { enabled_ok as f64 / enabled_valid as f64 };
        let disabled_rate = if disabled_valid == 0 { 0.0 } else { disabled_ok as f64 / disabled_valid as f64 };
        eprintln!(
            "{}: with unit {enabled_ok}/{enabled_valid} ({enabled_rate:.3}), without {disabled_ok}/{disabled_valid} ({disabled_rate:.3}), required >= {min_rate:.3}",
            scenario.name
        );
        for failure in &enabled_failures {
            eprintln!("FAIL [{} with unit]: {failure}", scenario.name);
        }
        for error in &judge_errors {
            eprintln!("ERROR [{}]: {error}", scenario.name);
        }
        if judge_errors.len() * 5 > trials * 2 {
            failures.push(format!(
                "{} had {} judge pipeline error(s) of {} samples; the rate is not trustworthy",
                scenario.name,
                judge_errors.len(),
                trials * 2
            ));
        }
        if scenario.accepted {
            if enabled_rate < min_rate {
                failures.push(format!(
                    "{} with unit {enabled_ok}/{enabled_valid} ({enabled_rate:.3} < {min_rate:.3})",
                    scenario.name
                ));
            }
        } else {
            eprintln!(
                "TENTATIVE {}: measured {enabled_rate:.3} with the unit; not accepted as effective",
                scenario.name
            );
        }
        if enabled_rate <= disabled_rate {
            eprintln!(
                "WARN {}: the unit did not change behavior versus the disabled baseline",
                scenario.name
            );
        }
        summary.push(serde_json::json!({
            "scenario": scenario.name,
            "unit_id": scenario.unit_id,
            "message": scenario.message,
            "trials": trials,
            "effective_with_unit": enabled_ok,
            "effective_without_unit": disabled_ok,
            "rate_with_unit": enabled_rate,
            "rate_without_unit": disabled_rate,
            "accepted": scenario.accepted,
            "discriminating": enabled_rate > disabled_rate,
            "judge_errors": judge_errors,
            "samples": observed,
            "failures_with_unit": enabled_failures,
            "failures_without_unit": disabled_failures,
        }));
    }
    eprintln!(
        "{}",
        serde_json::json!({
            "model": model,
            "baseline": "local served model (Qwen 3.8 27B NVFP4 class)",
            "sandbox_root": sandbox_root(),
            "trials_per_condition": trials,
            "min_rate": min_rate,
            "scenarios": summary,
        })
    );
    broker_task.abort();
    assert!(
        failures.is_empty(),
        "awareness did not produce effective results often enough: {}",
        failures.join("; ")
    );
}
