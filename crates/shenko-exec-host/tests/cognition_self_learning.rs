//! Real self-learning proof through the **actual default CLI**.
//!
//! ```text
//! SHENKO_USE_LOCAL_MODEL=1 SHENKO_LOCAL_MODEL_URL=<endpoint> SHENKO_LOCAL_MODEL=<served id> \
//! cargo test -p shenko-exec-host --test cognition_self_learning -- \
//!   --binary <built shenko> --repo <source checkout> [--root <fresh run dir>] --nocapture
//! ```
//!
//! The consumer does not build a broker, a subject loop, a native child or an
//! artifact of its own. It starts the built default CLI on a fresh
//! installation, submits one owner message through the product's own web-input
//! socket, watches the product's own task read model, stops the process through
//! its real SIGINT shutdown, and reads what the installation actually holds:
//! the authored body, artifact digest and revision, the learning record, the
//! adoption record, the original trial/judge task inputs and results, the
//! operation and attempt identities, the node's own trial evidence, and the
//! exact artifact bytes.
//!
//! What it never does: call `begin_trial`/`finish_trial`, install a canned or
//! pre-measured unit, score an answer with keywords, decide that a recorded
//! judgement is approval, or treat an unfinished/unknown attempt as efficacy.
//! A run in which nothing was measured, in which the judgement cites another
//! artifact, or in which the product refused the evidence is a *recorded
//! observation*, not a pass and not a fabricated failure.
//!
//! The message below is scripted by this consumer. It states the owner's
//! requirement and the evidence discipline; it does not contain the unit body,
//! the scenarios, the rubric or the verdict. Nothing here is autonomous
//! initiation, and the receipt says so.

#[path = "cognition_self_learning/harness.rs"]
mod harness;
#[path = "cognition_self_learning/facts.rs"]
mod facts;

use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    path::Path,
    time::{Duration, Instant},
};

/// The one owner message this consumer submits.
///
/// It names the real need — a durable, reusable judgement instead of a repeated
/// explanation — and the evidence discipline the product already requires. The
/// unit's body, applicability, cues, trial scenarios, rubric and decision are
/// the model's, not this consumer's.
const MESSAGE: &str = "我要你把这次学到的东西真正固定下来，而不是我以后每次重新解释。\n\
请你自己完成下面三件事：\n\
1. 把你认为值得长期复用的一件注意写成一则可独立安装、替换和学习的原子单元：它只承担一个注意点，写清它什么时候适用、什么时候不适用、你依据什么、以及这样判断的理由。写原则和理由，不要写成固定的动作脚本；理由要指到这次真实发生的观察或回执上。kind 由你选。你写下的是一段当前 harness 里的材料，它不是你因此就已经具备的注意能力。如果你还没判断过它有多可靠，就把把握记成未知，不要凑一个数字；如果记录带置信，写你真实的那个。\n\
2. 写完之后，用真实沙箱自测这个精确版本：自己设计一个能体现这则单元的场景和评审用的判据，让对照组真的不含这个候选版本。\n\
3. 然后依据那次自测返回的 attempt_id，登记你对这个精确版本的判断。要给出测量就说清依据的那次测量；如果测量没有结论、或者证据本身不成立，就如实把版本留在候选状态。不要用操作员陈述冒充测量，也不要为了让结果好看而写 adopt。\n\
完成后用一句话告诉我：这个版本现在是什么状态，它凭的是哪一条实际发生过的证据，以及你对它有多大的把握。";

fn write_new(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    use std::io::Write as _;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Watch the product's own task read model until the judge child settled or the
/// authoring deadline expires.
async fn watch_authoring(
    port: u16,
    deadline: Duration,
    timeline: &mut Vec<Value>,
) -> Result<String> {
    let started = Instant::now();
    let mut last: Option<String> = None;
    loop {
        let observed = harness::get_json(port, "/api/tasks?limit=200", Duration::from_secs(20)).await;
        match observed {
            Ok(value) => {
                let items = value
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let relevant: Vec<Value> = items
                    .into_iter()
                    .filter(|item| {
                        item.get("kind")
                            .and_then(Value::as_str)
                            .is_some_and(|kind| kind.starts_with("cognition.unit."))
                    })
                    .collect();
                let rendered = serde_json::to_string(&relevant).unwrap_or_default();
                if last.as_deref() != Some(rendered.as_str()) {
                    timeline.push(json!({
                        "at_ms": started.elapsed().as_millis() as u64,
                        "available": value.get("available"),
                        "note": value.get("note"),
                        "items": relevant,
                    }));
                    last = Some(rendered);
                }
                let judge_settled = relevant.iter().any(|item| {
                    item.get("kind").and_then(Value::as_str) == Some("cognition.unit.judge")
                        && item.get("state").and_then(Value::as_str) == Some("terminal")
                });
                if judge_settled {
                    return Ok("judge_task_terminal".to_owned());
                }
            }
            Err(error) => {
                timeline.push(json!({
                    "at_ms": started.elapsed().as_millis() as u64,
                    "error": format!("{error:#}"),
                }));
            }
        }
        if started.elapsed() > deadline {
            return Ok("authoring_deadline".to_owned());
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// The consumer is its own binary (`harness = false`): it takes `--binary` and
/// `--repo` directly and runs exactly one bounded proof per invocation.
fn main() {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cognition_self_learning: cannot start the runtime: {error}");
            std::process::exit(2);
        }
    };
    match runtime.block_on(run_consumer()) {
        Ok(()) => {}
        Err(error) => {
            eprintln!("cognition_self_learning: FAILED: {error:#}");
            std::process::exit(1);
        }
    }
}

async fn run_consumer() -> Result<()> {
    let Some((model_url, model)) = harness::Options::served_model() else {
        eprintln!(
            "SKIP cognition_self_learning: set SHENKO_USE_LOCAL_MODEL=1, \
             SHENKO_LOCAL_MODEL_URL and SHENKO_LOCAL_MODEL, and pass --binary"
        );
        return Ok(());
    };
    let options = harness::Options::from_args()?;
    ensure!(
        options.binary.is_file(),
        "the --binary path {} is not a file",
        options.binary.display()
    );
    if let Some(parent) = options.root.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::create_dir(&options.root).with_context(|| {
        format!(
            "create the fresh run directory {} (an existing directory is never reused)",
            options.root.display()
        )
    })?;
    let installation = options.root.join("installation");

    let binding = harness::source_binding(&options);
    write_new(&options.root.join("source-binding.json"), &binding)?;
    eprintln!(
        "cognition_self_learning: run directory {}\nbinary {}\nmodel {model} at {model_url}",
        options.root.display(),
        options.binary.display()
    );

    // Phase 1 — the installation as the default CLI leaves it, before any
    // owner message. This is the real "before" of the authored candidate.
    let mut baseline_server = harness::Server::start(&options, &installation, "baseline")?;
    let baseline_ready = baseline_server.wait_ready(options.startup).await?;
    let baseline_stop = baseline_server.stop(options.shutdown).await?;
    let before = facts::snapshot(&installation, "baseline")?;
    write_new(&options.root.join("baseline.json"), &before)?;
    let subject_id = before
        .subject_agent_id
        .clone()
        .context("the default CLI never bound a subject in this installation")?;
    eprintln!(
        "baseline: {} unit(s), subject {subject_id}, stop {:?} exit {:?}",
        before.units.len(),
        baseline_stop.signal,
        baseline_stop.exit_code
    );

    // Phase 2 — the same installation, one scripted owner message, and the
    // subject's own writing, sandbox self-test and exact-version judgement.
    let mut author_server = harness::Server::start(&options, &installation, "authoring")?;
    let author_ready = author_server.wait_ready(options.startup).await?;
    let submission = harness::submit(
        author_server.port,
        &subject_id,
        MESSAGE,
        Duration::from_secs(120),
    )
    .await?;
    let stage = submission
        .get("receipt")
        .and_then(|receipt| receipt.get("stage"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    ensure!(
        stage == "accepted",
        "the product did not durably accept the owner message: {}",
        serde_json::to_string_pretty(&submission)?
    );
    let mut timeline = Vec::new();
    let watched = watch_authoring(author_server.port, options.authoring, &mut timeline).await?;
    let author_stop = author_server.stop(options.shutdown).await?;
    let after = facts::snapshot(&installation, "after_authoring")?;
    write_new(&options.root.join("after-authoring.json"), &after)?;
    let classification = facts::classify(&before, &after);
    eprintln!(
        "authoring: {watched}; {} authored unit(s); outcome {}",
        classification.authored_unit_ids.len(),
        classification.outcome
    );

    // Phase 3 — restart the same installation and prove the authored versions
    // were not rewritten, superseded or re-judged by starting again.
    let mut restart_server = harness::Server::start(&options, &installation, "restart")?;
    let restart_ready = restart_server.wait_ready(options.startup).await?;
    tokio::time::sleep(Duration::from_secs(20)).await;
    let restart_stop = restart_server.stop(options.shutdown).await?;
    let restarted = facts::snapshot(&installation, "after_restart")?;
    write_new(&options.root.join("after-restart.json"), &restarted)?;
    let parity = facts::restart_parity(
        &after,
        &restarted,
        &classification.authored_unit_ids,
    );

    let receipt = json!({
        "schema_version": 1,
        "consumer": "cognition_self_learning",
        "consumer_source_binding": binding,
        "message": {
            "text": MESSAGE,
            "scripted_by_consumer": true,
            "autonomous_initiation_claimed": false,
            "contains_unit_body": false,
            "contains_scenarios_or_rubric": false,
            "requests_one_atom": true,
            "requests_reason_pointers": true,
            "requests_unknown_confidence_when_unjudged": true,
            "prescribes_unit_kind": false,
            "treats_authored_text_as_capability": false,
        },
        "model": { "url": model_url, "served_id": model },
        "run_directory": options.root.display().to_string(),
        "installation": installation.display().to_string(),
        "subject_agent_id": subject_id,
        "phases": {
            "baseline": { "ready": baseline_ready, "stop": baseline_stop },
            "authoring": {
                "ready": author_ready,
                "stop": author_stop,
                "submission": submission,
                "watch": watched,
                "task_timeline": timeline,
            },
            "restart": { "ready": restart_ready, "stop": restart_stop },
        },
        "before": {
            "unit_ids": before.units.iter().filter_map(|unit| unit.get("unit_id").cloned()).collect::<Vec<_>>(),
            "unit_count": before.units.len(),
            "task_count": before.tasks.len(),
        },
        "after": {
            "unit_count": after.units.len(),
            "task_count": after.tasks.len(),
            "evidence_errors": after.evidence_errors,
            "task_errors": after.task_errors,
        },
        "classification": classification.clone(),
        "restart_parity": parity.clone(),
        "cognitive_metacontract": {
            "clauses_read": [
                "SPEC \u{a7}51.1", "SPEC \u{a7}51.2", "SPEC \u{a7}51.3", "SPEC \u{a7}51.4",
                "SPEC \u{a7}51.5", "SPEC \u{a7}51.6", "SPEC \u{a7}51.7", "SPEC \u{a7}51.8",
                "SPEC \u{a7}51.9", "SPEC \u{a7}51.10",
                "docs/decisions/0002-cognitive-atoms-2026-11-05.md",
                "docs/decisions/0003-spec-governance-2026-11-06.md",
            ],
            "what_is_recorded": [
                "the exact content digest and the bytes this installation holds, re-hashed",
                "the attitude the ledger holds towards that one revision, verbatim when the installation publishes a canonical one",
                "the confidence exactly as recorded: absent stays {\"state\":\"absent\"}, an explicit {\"state\":\"unknown\"} stays unknown, a stated value stays stated",
                "the reason pointers verbatim and the terminations this consumer reads off them",
                "whether a reason passed through someone else's perception, which is not the subject's own observation",
                "the evidence verification tri-state (verified_local / refuted_local / undetermined) from the real verifier, labelled when only a legacy flag is available",
                "the revision disclosure when the installation publishes one, otherwise omitted/unresolved",
            ],
            "canonical_api_read": {
                "types": "shenko_types::cognition::{ContentRecord, ContentAttitude, AttitudeConfidence, ReasonPointer, ReasonTermination, ContentRevisionDisclosure}; shenko_store::atoms reads",
                "status": "published provisional-frozen by AlignAtoms180",
                "attitude_source_values": ["canonical_content_attitude", "legacy_adoption_record", "none"],
                "reason_reading": "derived by this consumer from the reason terminations; never read as a stored field",
                "awaits_actual_serializer_output": true,
                "expects_inline_row_mutation": false,
            },
            "taste_policy_implemented_here": false,
            "not_claimed": [
                "this consumer implements no attitude/reason-pointer record and no atoms store: it reads what the installation publishes and labels a legacy adoption record as legacy instead of dressing it up as a ContentAttitude",
                "evidence is never inferred from a non-empty reason list; an absent canonical tri-state stays undetermined and a legacy evidence_verified flag is labelled as a legacy flag",
                "no historical row is expected to mutate: the consumer only reads, and an inline legacy row that keeps its bytes is the expected outcome, not a failure",
                "harness text is not awareness capability (\u{a7}51.2): the submitted message and the authored bytes grant no authority and dispatch nothing",
                "the restart is a reread of a stopped installation, not cognitive rollback; no append-node versioned root was created, exercised or verified (\u{a7}51.4/\u{a7}51.10)",
                "no `cap.` capability contract, metapackage kind or dependency resolution was invented or claimed by this consumer (\u{a7}51.8)",
                "no claim about consciousness, feeling or legal personality (\u{a7}51.5)",
            ],
        },
        "scope_limits": [
            "the consumer drove the built default CLI through its own web-input socket and task read model; it did not re-implement or replace the product's composition",
            "the submitted message is scripted by the consumer: this run is not autonomous initiation and is not evidence that the agent initiates learning by itself",
            "a recorded judgement is a fact of this installation, not an approval decision by the consumer; empirical acceptance is reported only when the installation's own record carries empirical=true, its evidence state reads verified_local, and the cited attempt measured this exact artifact",
            "an unfinished, unknown or unexecuted attempt is never treated as efficacy; the product's own error code and the node's own measurement classification are retained verbatim",
            "the trial comparison is one bounded measurement on one served model at one moment; it does not prove the unit permanently effective",
            "the restart phase re-reads a stopped installation; it is not a crash or durability proof and not \u{a7}51.4 cognitive rollback",
            "the authored text is the current awareness harness, not an installed awareness capability (\u{a7}51.2); this run cannot show that reading it makes the subject notice anything",
            "the runtime still has no metapackage kind (\u{a7}51.6); where this installation publishes no canonical ContentAttitude the receipt says so rather than implementing \u{a7}51 in the consumer",
            "the reason terminations, confidence and evidence state come from the installation's own records; a legacy record with no canonical fields is reported as legacy, an absent confidence stays absent, an absent evidence state stays undetermined, and neither is defaulted",
        ],
    });
    write_new(&options.root.join("receipt.json"), &receipt)?;

    eprintln!(
        "cognition_self_learning: source-truth outcome {} (empirical acceptance {})\nretained facts: {}",
        classification.outcome,
        classification.empirical_acceptance,
        options.root.display()
    );
    for check in &classification.checks {
        eprintln!(
            "  [{}] {}: {}",
            if check.passed { "ok" } else { "not-proven" },
            check.name,
            check.detail
        );
    }
    if classification.empirical_acceptance
        && parity
            .get("no_rewrite_across_restart")
            .and_then(Value::as_bool)
            != Some(true)
    {
        bail!(
            "the run recorded an empirical acceptance but a restart changed the authored facts: {}",
            serde_json::to_string_pretty(&parity)?
        );
    }
    Ok(())
}
