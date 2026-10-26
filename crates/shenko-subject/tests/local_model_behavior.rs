//! Local-model behavioral acceptance (SPEC §5.1, §22.1, R077).
//!
//! Shenko is driven by a local model, so a step can be "covered" by Rust tests
//! and still be unusable: the model may never emit the control line, or may
//! emit one whose target it could not know. This test measures that directly.
//! It sends the product's own system prompt and staged message text through
//! the product's own provider client, parses the reply with the product's own
//! parser, and samples the model several times per behavior.
//!
//! It is model-agnostic: any served OpenAI-compatible chat model works, and the
//! test skips cleanly when no endpoint is configured so `cargo test` stays
//! hermetic. Passing is not "the feature works"; it is "this local model adapts
//! to the shipped behavior often enough to build on".
//!
//! ```sh
//! SHENKO_LOCAL_MODEL_URL=http://127.0.0.1:8000 \
//! SHENKO_LOCAL_MODEL=<served-model-id> \
//! SHENKO_BEHAVIOR_SAMPLES=8 SHENKO_BEHAVIOR_MIN_RATE=0.85 \
//! cargo test -p shenko-subject --test local_model_behavior -- --nocapture
//! ```

use shenko_broker::local_provider::LocalModelProvider;
use shenko_broker::request::{
    InferenceMessage, InferencePayload, InferencePriority, InferenceRequest,
};
use shenko_identity::{DefaultIdentityPolicy, IdentityService};
use shenko_subject::output::parse_output;
use shenko_subject::{IDLE_CONTINUATION_PROMPT, canonical_system_prompt, staged_message_text};
use shenko_types::ids::{ControllerEpoch, EntityId};
use shenko_types::object::{Digest, ObjectRef, SchemaRef};
use shenko_types::output::{OutputAction, ParsedOutput, PublicationTarget};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

/// The private account the fixture conversation claims to come from.
const REPLY_ACCOUNT: u64 = 10001;

fn private_message(text: &str) -> serde_json::Value {
    serde_json::json!({
        "post_type": "message",
        "message_type": "private",
        "self_id": 10000,
        "user_id": REPLY_ACCOUNT,
        "sender": { "user_id": REPLY_ACCOUNT, "nickname": "阿岚" },
        "raw_message": text,
    })
}

/// One behavior the shipped contract requires, with its staged events.
struct Scenario {
    name: &'static str,
    events: Vec<serde_json::Value>,
    check: fn(&str, &ParsedOutput) -> Result<(), String>,
}

fn reply_uses_the_account_in_the_header(raw: &str, parsed: &ParsedOutput) -> Result<(), String> {
    let replies: Vec<_> = parsed
        .controls
        .iter()
        .filter_map(|control| match &control.action {
            OutputAction::Reply { target, text } => Some((target.clone(), text.clone())),
            _ => None,
        })
        .collect();
    if replies.len() != 1 {
        return Err(format!(
            "expected exactly one reply control, found {}: {raw}",
            replies.len()
        ));
    }
    let (target, text) = &replies[0];
    if *target != PublicationTarget::QqPrivate(REPLY_ACCOUNT) {
        return Err(format!(
            "reply target {target:?} is not the account given in the message header: {raw}"
        ));
    }
    if text.trim().is_empty() {
        return Err(format!("reply body is empty: {raw}"));
    }
    Ok(())
}

fn remember_keeps_the_stated_fact(raw: &str, parsed: &ParsedOutput) -> Result<(), String> {
    let remembered: Vec<String> = parsed
        .controls
        .iter()
        .filter_map(|control| match &control.action {
            OutputAction::Remember { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    if remembered.len() != 1 {
        return Err(format!(
            "expected exactly one [remember] control, found {}: {raw}",
            remembered.len()
        ));
    }
    let normalized: String = remembered[0]
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    if !normalized.contains("3月14") {
        return Err(format!(
            "[remember] did not keep the stated fact (3 月 14): {raw}"
        ));
    }
    Ok(())
}

fn idle_keeps_thinking(raw: &str, parsed: &ParsedOutput) -> Result<(), String> {
    let replies = parsed
        .controls
        .iter()
        .filter(|control| matches!(control.action, OutputAction::Reply { .. }))
        .count();
    if replies != 0 {
        return Err(format!(
            "idle step invented {replies} reply target(s) with no external message: {raw}"
        ));
    }
    let prose: usize = parsed.text.chars().filter(|ch| !ch.is_whitespace()).count();
    if prose < 20 {
        return Err(format!("idle step produced no continuing thought: {raw}"));
    }
    Ok(())
}

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "reply_to_private_message",
            events: vec![private_message("你在吗？周末有空一起去看展览吗")],
            check: reply_uses_the_account_in_the_header,
        },
        Scenario {
            name: "remember_a_stated_fact",
            events: vec![private_message("记住：我的生日是 3 月 14 日")],
            check: remember_keeps_the_stated_fact,
        },
        Scenario {
            name: "idle_continuation",
            events: Vec::new(),
            check: idle_keeps_thinking,
        },
    ]
}

fn messages_for(scenario: &Scenario) -> Vec<InferenceMessage> {
    if scenario.events.is_empty() {
        return vec![InferenceMessage::text("user", IDLE_CONTINUATION_PROMPT)];
    }
    scenario
        .events
        .iter()
        .map(|event| InferenceMessage::text("user", staged_message_text(event, "napcat")))
        .collect()
}

fn raw_object(raw: &str) -> ObjectRef {
    ObjectRef {
        digest: Digest::blake3(raw.as_bytes()),
        bytes: raw.len() as u64,
        media_type: "text/plain".into(),
        store_namespace: "model-behavior".into(),
        locator: "model-behavior:unretained-output".into(),
        codec: SchemaRef {
            name: "raw".into(),
            major: 1,
            digest: Digest::blake3(b"raw"),
        },
    }
}

#[tokio::test(flavor = "current_thread")]
async fn local_model_follows_the_shipped_control_protocol() {
    let (Ok(base_url), Ok(model)) = (
        std::env::var("SHENKO_LOCAL_MODEL_URL"),
        std::env::var("SHENKO_LOCAL_MODEL"),
    ) else {
        eprintln!(
            "SKIP local_model_behavior: set SHENKO_LOCAL_MODEL_URL and SHENKO_LOCAL_MODEL \
             to a served OpenAI-compatible model to measure local model behavior"
        );
        return;
    };
    let samples: usize = std::env::var("SHENKO_BEHAVIOR_SAMPLES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8)
        .clamp(1, 32);
    let min_rate: f64 = std::env::var("SHENKO_BEHAVIOR_MIN_RATE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.85)
        .clamp(0.0, 1.0);

    let provider = LocalModelProvider::new(base_url.clone(), model.clone());
    let binding = provider.binding();
    let identity = IdentityService::new(Arc::new(DefaultIdentityPolicy));
    let profile = DefaultIdentityPolicy::genesis(EntityId::new(), Some("小满"));
    let mut system_prompt = canonical_system_prompt();
    system_prompt.push_str("\n\n");
    system_prompt.push_str(&identity.persona_section(&profile));
    let prompt_digest = Digest::blake3(system_prompt.as_bytes());

    let mut failures: Vec<String> = Vec::new();
    let mut results: Vec<serde_json::Value> = Vec::new();
    for scenario in scenarios() {
        let messages = messages_for(&scenario);
        let mut passed = 0_usize;
        let mut observed_failures: Vec<String> = Vec::new();
        for _ in 0..samples {
            let (result_tx, _result_rx) = oneshot::channel();
            let request = InferenceRequest {
                request_id: EntityId::new(),
                agent_id: EntityId::new(),
                conversation_id: EntityId::new(),
                provider_binding: binding.clone(),
                priority: InferencePriority::Subject,
                epoch: ControllerEpoch(1),
                payload: InferencePayload {
                    system_prompt: system_prompt.clone(),
                    messages: messages.clone(),
                    max_tokens: Some(4096),
                    tools: Vec::new(),
                    native_options: Default::default(),
                },
                result_tx,
            };
            let attempt = match tokio::time::timeout(
                Duration::from_secs(180),
                provider.complete(&request),
            )
            .await
            {
                Ok(Ok(response)) => {
                    let parsed = parse_output(
                        request.request_id,
                        raw_object(&response.content),
                        &response.content,
                        &identity,
                    );
                    match parsed {
                        Ok(parsed) => (scenario.check)(&response.content, &parsed),
                        Err(error) => {
                            Err(format!("output does not parse: {error}: {}", response.content))
                        }
                    }
                }
                Ok(Err(error)) => Err(format!("provider call failed: {error}")),
                Err(_) => Err("provider call exceeded 180s".to_string()),
            };
            match attempt {
                Ok(()) => passed += 1,
                Err(failure) => {
                    let failure: String = failure.chars().take(600).collect();
                    eprintln!("FAIL [{}]: {failure}", scenario.name);
                    observed_failures.push(failure);
                }
            }
        }
        let rate = passed as f64 / samples as f64;
        eprintln!(
            "{}: {passed}/{samples} ({rate:.3}) required >= {min_rate:.3}",
            scenario.name
        );
        if rate < min_rate {
            failures.push(format!(
                "{} passed {passed}/{samples} (rate {rate:.3} < {min_rate:.3})",
                scenario.name
            ));
        }
        results.push(serde_json::json!({
            "scenario": scenario.name,
            "passed": passed,
            "samples": samples,
            "rate": rate,
            "failures": observed_failures,
        }));
    }
    eprintln!(
        "{}",
        serde_json::json!({
            "model": model,
            "base_url": base_url,
            "prompt_digest": prompt_digest.0,
            "samples_per_scenario": samples,
            "min_rate": min_rate,
            "scenarios": results,
        })
    );
    assert!(
        failures.is_empty(),
        "local model did not adapt to the shipped behavior: {}",
        failures.join("; ")
    );
}
