//! The S0 boundary: durable raw facts -> committed window -> native payload.
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use shenko_broker::request::{InferenceMessage, InferencePart, InferencePayload, ProviderCapabilities};
use shenko_context_policy::{
    Attempt, CONTRACT_VERSION, CommittedWindow, HistoryItem, HistorySource, ItemKind, ModelCaps,
    PrepareError, Projected, StorageError, StoreHistory, VisionCaps, estimate_tokens,
};
use shenko_store::{Store, SubjectContextRef, SubjectHistoryInput};
use shenko_types::{error::ShenkoError, ids::EntityId, object::Digest, task::TaskResult};

use crate::inbox::{InboxEvent, InboxEventKind};
use crate::loop_runner::{SubjectLoopState, build_system_prompt, stable_system_prompt};

/// The user turn the product adds when a step has no external event. It keeps
/// the thought loop going without inventing an instruction, and behavior
/// evaluation samples this exact sentence.
pub const IDLE_CONTINUATION_PROMPT: &str = "[运行时连续思考提示；不是新外部消息] 继续。";

#[derive(Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(super) enum ContextSelection {
    Unavailable { reason: String },
    Retained,
    NoUsablePolicy { attempts: Vec<Attempt> },
    Compacted { attempts: Vec<Attempt> },
}

#[derive(Serialize, Deserialize)]
pub(super) struct FrozenContextInput {
    pub source: SubjectContextRef,
    pub profile_digest: Digest,
    pub capabilities: Option<Arc<ProviderCapabilities>>,
    pub observed_input_tokens: Option<u32>,
    pub estimated_input_tokens: Option<u32>,
    pub budget_input_tokens: Option<u32>,
    pub trigger_budget_input_tokens: Option<u32>,
    pub budget_provenance: String,
    pub reserved_output_tokens: u32,
    pub unfinished_call_ids: Vec<String>,
    pub token_counter: String,
    pub policy_contract: u32,
    pub selection: ContextSelection,
}

pub(super) struct PreparedContextInput {
    pub events: Vec<InboxEvent>,
    pub semantic_wait: Option<shenko_types::subject::SemanticWait>,
    pub payload: InferencePayload,
    pub context: FrozenContextInput,
}

fn storage(error: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::storage(error.to_string())
}

/// Bound the append-only source before selecting/projecting a window. A later
/// append is rejected by the final Store S0 CAS, never silently added here.
struct BoundedHistory<'a> {
    history: &'a StoreHistory,
    leaf: Option<u64>,
}

impl HistorySource for BoundedHistory<'_> {
    fn leaf_seq(&self) -> Result<Option<u64>, StorageError> { Ok(self.leaf) }

    fn read_range(&self, from: u64, to: u64) -> Result<Vec<HistoryItem>, StorageError> {
        match self.leaf {
            Some(leaf) if from <= leaf => self.history.read_range(from, to.min(leaf)),
            _ => Ok(Vec::new()),
        }
    }

    fn read_event(&self, id: EntityId) -> Result<Option<HistoryItem>, StorageError> {
        Ok(self.history.read_event(id)?.filter(|item| self.leaf.is_some_and(|leaf| item.at.seq <= leaf)))
    }
}

/// The visible text one accepted ingress event contributes to the model
/// history.
///
/// The header carries the speaker and the reply target the platform actually
/// reported. A local model can only emit `[reply:private:QQ号]` /
/// `[reply:group:群号]` with a number it has seen, so omitting it would make a
/// control that parses correctly yet can never be executed. This is a
/// projection only: `inbox_event` keeps the untrusted payload verbatim, and an
/// event that declares no account keeps the plain `[speaker]` header.
pub fn staged_message_text(payload: &serde_json::Value, source: &str) -> String {
    let sender = payload.get("sender");
    let who = sender
        .and_then(|sender| sender.get("nickname"))
        .or_else(|| payload.get("nickname"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|who| !who.is_empty())
        .unwrap_or(source);
    let body = ["raw_message", "message", "content", "text"]
        .iter()
        .find_map(|key| payload.get(*key).and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| payload.to_string());
    let account = |key: &str| {
        payload
            .get(key)
            .or_else(|| sender.and_then(|sender| sender.get(key)))
            .and_then(serde_json::Value::as_u64)
            .filter(|id| *id > 0)
    };
    let target = match payload.get("message_type").and_then(serde_json::Value::as_str) {
        Some("group") => account("group_id").map(|id| format!("群 {id}")),
        Some("private") => account("user_id").map(|id| format!("私聊 QQ {id}")),
        _ => None,
    };
    match target {
        Some(target) => format!("[{who} · {target}] {body}"),
        None => format!("[{who}] {body}"),
    }
}

fn stage_input(state: &SubjectLoopState, event: &InboxEvent) -> Result<SubjectHistoryInput, ShenkoError> {
    let mut data = serde_json::json!({"v":1, "inbox_event":event});
    let event_type = if matches!(&event.kind, InboxEventKind::TaskResult) && event.source == "native-child" {
        // An inbox claim is not execution evidence. Bind it to the actual T2
        // entry before giving it a tool-result role or an operation link.
        let claimed = TaskResult::deserialize(&event.payload)?;
        let result = state.store.native_task_result(claimed.task_id).map_err(storage)?
            .ok_or_else(|| ShenkoError::conflict("native inbox result has no T2 entry"))?;
        let task = state.store.external_task(result.task_id).map_err(storage)?;
        if task.agent_id != state.agent_id || result.result_id != event.event_id
            || result != claimed
        {
            return Err(ShenkoError::conflict("native inbox result/T2 identity mismatch"));
        }
        data["text"] = serde_json::Value::String(serde_json::to_string(&result)?);
        data["call_id"] = serde_json::json!(result.task_id);
        data["operation_id"] = serde_json::json!(result.operation_id);
        if let Some(facts) = state.store.subject_call_evidence(EntityId(state.head.branch_id.0), result.task_id)? {
            if facts.dispatch_state != "settled" {
                return Err(ShenkoError::conflict("native task result before intent settlement"));
            }
            data["intent_id"] = serde_json::json!(result.task_id);
        }
        "tool_result"
    } else {
        data["text"] = serde_json::Value::String(staged_message_text(&event.payload, &event.source));
        // Untrusted ingress kinds never acquire system/assistant authority.
        "user"
    };
    Ok(SubjectHistoryInput { event_id: event.event_id, event_type: event_type.into(), data })
}

fn staged_original(data: &str) -> Result<InboxEvent, ShenkoError> {
    let mut data: serde_json::Value = serde_json::from_str(data)?;
    let original = data.get_mut("inbox_event").map(serde_json::Value::take)
        .ok_or_else(|| ShenkoError::conflict("staged original inbox event missing"))?;
    Ok(serde_json::from_value(original)?)
}

fn same_source_delivery(left: &InboxEvent, right: &InboxEvent) -> bool {
    left.event_id == right.event_id && left.kind == right.kind
        && left.payload == right.payload && left.source == right.source
}

pub(super) fn estimate_payload_tokens(payload: &InferencePayload, vision_tokens: u32) -> Option<u32> {
    let mut estimate = estimate_tokens(&payload.system_prompt);
    let mut has_image = false;
    for part in payload.messages.iter().flat_map(|message| &message.parts) {
        match part {
            InferencePart::Text { text } => estimate = estimate.saturating_add(estimate_tokens(text)),
            InferencePart::Image { .. } => has_image = true,
        }
    }
    if has_image {
        (vision_tokens > 0).then(|| estimate.saturating_add(vision_tokens))
    } else {
        Some(estimate)
    }
}

fn project_payload(
    window: &CommittedWindow,
    history: &dyn HistorySource,
    base_system_prompt: String,
    caps: Option<&ModelCaps>,
    max_tokens: u64,
    fresh_ids: &[EntityId],
) -> Result<(InferencePayload, Option<u32>, Vec<String>), ShenkoError> {
    let mut system_prompt = base_system_prompt;
    let mut messages = Vec::new();
    let mut unfinished = std::collections::BTreeSet::new();
    // Policy derivatives may summarize old experience, but never constitute
    // first presentation of a pending input. Keep the derivative's provenance
    // and follow it with the pending raw bodies in their original order.
    // This is a subject delivery invariant, independent of installed policies.
    let mut fresh_items = std::collections::BTreeMap::new();
    for id in fresh_ids {
        let item = history.read_event(*id).map_err(storage)?
            .ok_or_else(|| ShenkoError::conflict("pending raw input missing from bounded history"))?;
        fresh_items.insert(item.at.seq, item);
    }
    let mut projection = Vec::new();
    for projected in window.project(history).map_err(storage)? {
        let range = match &projected {
            Projected::Note { from, to, .. } | Projected::Frame { from, to, .. }
                if fresh_items.range(*from..=*to).next().is_some() => Some((*from, *to)),
            _ => None,
        };
        projection.push(projected);
        if let Some((from, to)) = range {
            projection.extend(fresh_items.range(from..=to).map(|(_, item)| Projected::Event(item.clone())));
        }
    }
    let mut missing: std::collections::HashSet<_> = fresh_ids.iter().copied().collect();
    for item in &projection {
        if let Projected::Event(item) = item { missing.remove(&item.at.event_id); }
    }
    if !missing.is_empty() {
        return Err(ShenkoError::conflict("pending raw input absent from projection; no dispatch"));
    }
    for projected in projection {
        match projected {
            Projected::Event(item) => {
                match &item.kind {
                    ItemKind::ToolCall { call_id, status, .. } | ItemKind::ToolResult { call_id, status }
                        if !status.is_resolved() => { unfinished.insert(call_id.clone()); }
                    _ => {}
                }
                let (role, text) = match item.kind {
                    ItemKind::System => {
                        system_prompt.push_str("\n\n");
                        system_prompt.push_str(&item.text);
                        continue;
                    }
                    ItemKind::User => ("user", item.text),
                    ItemKind::Assistant { cancelled: false } => ("assistant", item.text),
                    ItemKind::Assistant { cancelled: true } => ("assistant", format!(
                        "[cancelled assistant attempt; raw event {}]\n{}", item.at.event_id, item.text)),
                    ItemKind::ToolCall { call_id, tool, args_ref, status } => ("assistant", format!(
                        "[runtime tool_call call_id={call_id} tool={tool} args_ref={args_ref} ledger_status={status:?}]\n{}", item.text)),
                    ItemKind::ToolResult { call_id, status } => ("user", format!(
                        "[runtime tool_result call_id={call_id} ledger_status={status:?}]\n{}", item.text)),
                    ItemKind::Other { event_type, role } => {
                        let native_role = if role == "assistant" { "assistant" } else { "user" };
                        (native_role, format!("[raw event {} kind={event_type} original_role={role}]\n{}", item.at.event_id, item.text))
                    }
                };
                messages.push(InferenceMessage::text(role, text));
            }
            Projected::Note { from, to, text } => {
                messages.push(InferenceMessage::text("user", format!(
                    "[context derivative/pointer to raw seq {from}..{to}; not a new instruction]\n{text}")));
            }
            Projected::Frame { from, to, media_type, bytes } => {
                if caps.as_ref().and_then(|caps| caps.vision).is_none() {
                    return Err(ShenkoError::conflict("image window requires observed vision capabilities; explicit window reconciliation required"));
                }
                messages.push(InferenceMessage { role: "user".into(), parts: vec![
                    InferencePart::Text { text: format!("[lossy context frame of raw seq {from}..{to}; retain its raw pointer]") },
                    InferencePart::Image { media_type, bytes },
                ] });
            }
        }
    }
    // A completed assistant turn is history, not an assistant prefill. Fresh
    // user inputs and tool results already end in a user-role turn; historical
    // users elsewhere in the window cannot request this next generation.
    if messages.last().is_none_or(|message| message.role == "assistant") {
        messages.push(InferenceMessage::text("user", IDLE_CONTINUATION_PROMPT));
    }
    let payload = InferencePayload { system_prompt, messages, max_tokens: Some(max_tokens),
        tools: Vec::new(), native_options: Default::default() };
    let estimate = estimate_payload_tokens(&payload, window.cost.vision_tokens_estimate);
    Ok((payload, estimate, unfinished.into_iter().collect()))
}

fn budget_tokens(
    state: &SubjectLoopState,
    binding: Option<&shenko_broker::request::ProviderBinding>,
    current: u32,
) -> (u32, String) {
    let anchored = binding.is_some() && state.previous_input_binding.as_ref() == binding;
    match (anchored, state.previous_input_tokens, state.previous_estimated_input_tokens) {
        (true, Some(actual), Some(previous)) => (
            current.max(actual.saturating_add(current.saturating_sub(previous))),
            format!("max(current heuristic, previous provider input {actual} + positive delta from previous heuristic {previous})"),
        ),
        _ => (current, "current heuristic; no matching previous provider/estimate anchor".into()),
    }
}

pub(super) fn project_payload_stable(
    store: &Store,
    window: &CommittedWindow,
    history: &dyn HistorySource,
    base_system_prompt: String,
    caps: Option<&ModelCaps>,
    max_tokens: u64,
    fresh_ids: &[EntityId],
) -> Result<(InferencePayload, Option<u32>, Vec<String>, u64), ShenkoError> {
    // The revision is acquired BEFORE any execution-status read. Equality
    // after projection is what permits this exact revision to enter the seal.
    let source_commit_seq = store.current_commit_seq()?;
    let (payload, estimate, unfinished) = project_payload(
        window, history, base_system_prompt, caps, max_tokens, fresh_ids,
    )?;
    if store.current_commit_seq()? != source_commit_seq {
        return Err(ShenkoError::ResnapshotRequired {
            reason: "execution facts changed during projection".into(),
        });
    }
    Ok((payload, estimate, unfinished, source_commit_seq))
}

pub(super) fn prepare_context(
    state: &mut SubjectLoopState,
    events: &[InboxEvent],
) -> Result<PreparedContextInput, ShenkoError> {
    // Observe the durable cap before taking this new S0 snapshot. New input is
    // evidence for the model, not an automatic semantic-wait exit.
    state.semantic_wait = state.store.observe_semantic_wait(&state.head)?;
    // Delivery timestamps describe each volatile replay, not a new source
    // fact. Recover the first durable original and never overwrite it with a
    // replay's Utc::now(). Every stable source field still has to match.
    let already_staged = state.store.stage_subject_inputs(&state.head, &state.step, &[])?;
    let mut originals = std::collections::HashMap::new();
    let mut source_ids = std::collections::HashMap::new();
    for record in already_staged {
        let original = staged_original(&record.data)?;
        source_ids.insert(original.event_id, record.event_id);
        originals.insert(record.event_id, original);
    }
    let mut pending = std::collections::HashMap::new();
    let mut inputs = Vec::new();
    for event in events {
        let duplicate = source_ids.get(&event.event_id)
            .is_some_and(|raw| same_source_delivery(&originals[raw], event))
            || pending.get(&event.event_id)
                .is_some_and(|original| same_source_delivery(*original, event));
        if !duplicate {
            pending.insert(event.event_id, event);
            inputs.push(stage_input(state, event)?);
        }
    }
    let records = state.store.stage_subject_inputs(&state.head, &state.step, &inputs)?;
    let mut recovered = Vec::with_capacity(records.len());
    let mut input_raw_event_ids = Vec::with_capacity(records.len());
    for record in records {
        recovered.push(match originals.remove(&record.event_id) {
            Some(original) => original,
            None => staged_original(&record.data)?,
        });
        input_raw_event_ids.push(record.event_id);
    }
    // Awareness is installed material, not a keyword lookup: the model receives
    // every enabled perspective (bounded) and decides what, if anything, it means
    // here. A word-matching gate would withhold the unit precisely when the
    // message names a symptom instead of the concept.
    let awareness: Vec<String> = state
        .awareness
        .as_ref()
        .map(|registry| registry.active_intuitions())
        .unwrap_or_default();
    let stable_prompt = stable_system_prompt(state);
    let system_prompt = build_system_prompt(state, &awareness);
    let registry = Arc::clone(&state.context_registry);
    let binding = state.broker.as_ref().map(|broker| broker.binding());
    if state.model_capabilities.as_ref().is_some_and(|caps| Some(&caps.binding) != binding) {
        return Err(ShenkoError::conflict("context capabilities/provider binding mismatch"));
    }
    let profile_digest = Digest::blake3(&serde_json::to_vec(&(
        binding, &state.model_capabilities, &stable_prompt, state.max_tokens,
        "heuristic:cjk-1-other-4@1", CONTRACT_VERSION,
    ))?);
    let current_generation = registry.ledger().model_generation().map_err(storage)?;
    let profile_matches = registry.ledger().model_profile().map_err(storage)?
        .is_some_and(|(generation, profile)|
            generation == current_generation && profile.as_str() == profile_digest.0.as_str());
    if !profile_matches {
        registry.ledger().current().map_err(storage)?;
        let generation = current_generation
            .checked_add(1).filter(|value| i64::try_from(*value).is_ok())
            .ok_or_else(|| ShenkoError::conflict("context model generation exhausted"))?;
        registry.ledger().rebind_model(generation, &profile_digest.0).map_err(storage)?;
    }
    let model_generation = registry.ledger().model_generation().map_err(storage)?;
    let scope_id = EntityId(state.head.branch_id.0);
    let source = StoreHistory::new(Arc::clone(&state.store), scope_id, Arc::new(estimate_tokens));
    let history = BoundedHistory { leaf: source.leaf_seq().map_err(storage)?, history: &source };
    let advertised_caps = state.model_capabilities.as_ref().and_then(|caps|
        caps.max_context_tokens.filter(|limit| *limit > 0).map(|max_context| ModelCaps {
            model_id: caps.binding.model().into(), max_context,
            vision: caps.vision.as_ref().map(|vision| VisionCaps {
                max_image_context: vision.max_image_context, patch_px: vision.patch_px,
            }),
        })
    );
    let reserved_output_tokens = u32::try_from(state.max_tokens)
        .map_err(|_| ShenkoError::conflict("output reservation exceeds token budget type"))?;
    let caps = advertised_caps.as_ref().map(|caps| ModelCaps {
        model_id: caps.model_id.clone(),
        max_context: caps.max_context.saturating_sub(reserved_output_tokens),
        vision: caps.vision.map(|vision| VisionCaps {
            max_image_context: vision.max_image_context.saturating_sub(reserved_output_tokens),
            patch_px: vision.patch_px,
        }),
    });
    if caps.as_ref().is_some_and(|caps| caps.max_context == 0) {
        return Err(ShenkoError::conflict("output reservation leaves no input context"));
    }
    let mut window = registry.ledger().current().map_err(storage)?;
    let observed_input_tokens = state.previous_input_tokens.filter(|_|
        binding.is_some() && state.previous_input_binding.as_ref() == binding);
    let (mut payload, mut estimated_input_tokens, mut unfinished_call_ids, mut source_commit_seq) = project_payload_stable(
        &state.store, &window, &history, system_prompt.clone(), caps.as_ref(), state.max_tokens, &input_raw_event_ids,
    )?;
    let current_estimate = estimated_input_tokens
        .ok_or_else(|| ShenkoError::conflict("current projection has no trustworthy token estimate"))?;
    let (trigger_budget_input_tokens, mut budget_provenance) = budget_tokens(state, binding, current_estimate);
    let mut selected_budget_input_tokens = trigger_budget_input_tokens;
    let selection = match &caps {
        None => ContextSelection::Unavailable { reason: "model_context_limit_not_observed".into() },
        Some(caps) => {
            if !registry.should_compact(&history, caps, trigger_budget_input_tokens).map_err(storage)? {
                ContextSelection::Retained
            } else {
                match registry.prepare(&history, caps, trigger_budget_input_tokens) {
                    Ok((prepared, attempts)) => {
                        window = registry.commit(prepared, &history).map_err(storage)?;
                        (payload, estimated_input_tokens, unfinished_call_ids, source_commit_seq) = project_payload_stable(
                            &state.store, &window, &history, system_prompt, Some(caps), state.max_tokens, &input_raw_event_ids,
                        )?;
                        selected_budget_input_tokens = estimated_input_tokens
                            .ok_or_else(|| ShenkoError::conflict("compacted projection has no trustworthy token estimate"))?;
                        budget_provenance.push_str("; selected compacted heuristic");
                        ContextSelection::Compacted { attempts }
                    }
                    Err(PrepareError::NoUsablePolicy { attempts }) => ContextSelection::NoUsablePolicy { attempts },
                    Err(PrepareError::Storage(error)) => return Err(storage(error)),
                }
            }
        }
    };
    let final_estimate = estimated_input_tokens
        .ok_or_else(|| ShenkoError::conflict("selected projection has no trustworthy token estimate"))?;
    let has_image = payload.messages.iter().flat_map(|message| &message.parts)
        .any(InferencePart::is_image);
    let input_limit = caps.as_ref().and_then(|caps| if has_image {
        caps.image_request_context()
    } else {
        Some(caps.max_context)
    });
    if input_limit.is_some_and(|limit| selected_budget_input_tokens > limit || final_estimate > limit) {
        return Err(ShenkoError::conflict(format!(
            "projected input budget {selected_budget_input_tokens} exceeds available context {input_limit:?}; no dispatch"
        )));
    }
    Ok(PreparedContextInput {
        events: recovered, semantic_wait: state.semantic_wait.clone(),
        payload,
        context: FrozenContextInput {
            source: SubjectContextRef { scope_id, source_commit_seq,
                leaf_seq: history.leaf, input_raw_event_ids,
                window_id: window.id, window_generation: window.generation, model_generation },
            profile_digest, capabilities: state.model_capabilities.clone(),
            observed_input_tokens,
            estimated_input_tokens,
            budget_input_tokens: Some(selected_budget_input_tokens),
            trigger_budget_input_tokens: Some(trigger_budget_input_tokens),
            budget_provenance: format!("{budget_provenance}; reserved output {reserved_output_tokens}"),
            reserved_output_tokens,
            unfinished_call_ids,
            token_counter: "heuristic:cjk-1-other-4@1; not a provider tokenizer".into(),
            policy_contract: CONTRACT_VERSION, selection,
        },
    })
}
