//! The S0 boundary: durable raw facts -> committed window -> native payload.
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use shenko_broker::request::{InferenceMessage, InferencePart, InferencePayload, ProviderCapabilities};
use shenko_context_policy::{
    Attempt, CONTRACT_VERSION, CommittedWindow, HistoryItem, HistorySource, ItemKind, ModelCaps,
    Projected, StorageError, StoreHistory, VisionCaps, estimate_tokens,
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
    /// Compaction is preparing off the S0 path; this step keeps the current
    /// window. The next step adopts the prepared candidate when it verifies.
    Preparing { attempts: Vec<Attempt> },
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
    /// The prepared contributions collected for this step. They are sealed
    /// with the request, so each item's provenance (producer, version,
    /// artifact digest) reaches the raw call record through the seal.
    pub contributions: Vec<serde_json::Value>,
    /// Why each prepared contribution was NOT sent (conflict, expired,
    /// scope mismatch, over budget). An omission is data, not a failure.
    pub contribution_omissions: Vec<serde_json::Value>,
    /// The token cost the contributions charge against this step's budget.
    pub contribution_tokens: u64,
    /// The collected plan itself. It is carried to the seal because the
    /// usage evidence is built from it (the reduced use records and the
    /// omission reasons), and that evidence must name the request which
    /// actually carried the material.
    pub contribution_plan: Option<shenko_context_policy::ContributionPlan>,
}

fn storage(error: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::storage(error.to_string())
}

/// What the contribution registry returned for this step.
pub(super) struct CollectedContributions {
    pub contributions: Vec<serde_json::Value>,
    pub omissions: Vec<serde_json::Value>,
    pub total_tokens: u64,
    /// The model-facing text of every collected contribution, resolved from
    /// the sealed document each contribution's reference points at.
    pub material: Vec<String>,
    pub plan: shenko_context_policy::ContributionPlan,
}

/// Collect the prepared contributions for this step.
///
/// The registry merges them: identical material collapses, a disagreement is
/// reported, and a producer's failure never throws. An omission carries its
/// own reason and is DATA, not a failure of the step. A missing REQUIRED
/// contribution is the decision point: the step is not admissible without
/// it, so it fails with a typed conflict that names exactly what was
/// missing — never a silent drop.
fn collect_contributions(
    state: &SubjectLoopState,
    scope_id: EntityId,
) -> Result<Option<CollectedContributions>, ShenkoError> {
    let Ok(registry) = state
        .plugin_context
        .try_service::<shenko_context_policy::ContributionRegistry>()
    else {
        // No contribution runtime installed: there is nothing to collect.
        return Ok(None);
    };
    // The scope is scope-wide: a task-bound contribution is not eligible
    // for the subject's own step, and the registry reports that as a
    // scope_mismatch omission instead of including it silently.
    let scope = shenko_context_policy::contribution::scope_wide(scope_id);
    let plan = registry.collect(&scope, chrono::Utc::now().timestamp_millis());
    if !plan.missing_required.is_empty() {
        return Err(ShenkoError::conflict(format!(
            "CONTRIBUTION_REQUIRED_MISSING: {}",
            plan.missing_required.join(", ")
        )));
    }
    // The model-facing text of a contribution lives in the CAS document its
    // reference points at, not in the record itself. The document version is
    // read through the registry's own reader, so a newer document is an
    // error rather than a guess.
    let mut material = Vec::new();
    for contribution in &plan.contributions {
        let bytes = state.store.cas.read_object(&contribution.data.digest)?;
        if bytes.len() as u64 != contribution.data.bytes
            || Digest::blake3(&bytes) != contribution.data.digest
        {
            return Err(ShenkoError::ObjectClosureIncomplete { missing_count: 1 });
        }
        material.push(
            shenko_context_policy::contribution::ContributionPayload::read(&bytes).map_err(
                |error| {
                    ShenkoError::storage(format!("CONTRIBUTION_PAYLOAD_INVALID: {error}"))
                },
            )?,
        );
    }
    let contributions = plan
        .contributions
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(ShenkoError::Serde)?;
    let omissions = plan
        .omitted
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(ShenkoError::Serde)?;
    Ok(Some(CollectedContributions {
        contributions,
        omissions,
        total_tokens: plan.total_tokens,
        material,
        plan,
    }))
}

/// A prepared window can only be adopted against the history it was cut
/// from. Staleness of any binding (source window, model generation, policy
/// epoch, leaf) means this S0's snapshot is no longer the one the candidate
/// verified — re-snapshot, never adopt against a moved source. Storage
/// failures stay storage failures.
fn adopt_error(error: shenko_context_policy::CommitError) -> ShenkoError {
    match error {
        shenko_context_policy::CommitError::Storage(error) => storage(error),
        other => ShenkoError::ResnapshotRequired {
            reason: format!("prepared context window not adoptable: {other}"),
        },
    }
}

/// The slow compaction work (raw history render, per-policy attempts,
/// window cut) runs on the blocking pool, never inside an S0 seal. The
/// candidate publishes when done; a later step adopts it when it verifies
/// against that step's bounded history. Failure is logged, not propagated:
/// the current window stays live and the next over-budget step retries.
fn launch_context_preparer(
    registry: &Arc<shenko_context_policy::PolicyRegistry>,
    store: Arc<Store>,
    scope_id: EntityId,
    caps: ModelCaps,
    used_tokens: u32,
) {
    let registry = Arc::clone(registry);
    tokio::task::spawn_blocking(move || {
        let history =
            StoreHistory::new(store, scope_id, Arc::new(estimate_tokens));
        // Prepare and publish are one off-S0 step: the candidate binds itself
        // to the live scope/window/model/policy revisions it was cut from, so
        // a later adoption can verify every one of them.
        match registry.prepare_and_publish(&history, &caps, used_tokens) {
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "context preparer produced no publishable candidate");
            }
        }
    });
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
/// reported. A local model can only emit a registered `[reply:<kind>:<id>]`
/// control with an account it has seen, so omitting it would make a control
/// that parses correctly yet can never be executed. The channel plugin that
/// owns `source` decides which payload fields name a replyable audience;
/// unknown sources keep the plain `[speaker]` header. This is a projection
/// only: `inbox_event` keeps the untrusted payload verbatim, and an event
/// that declares no account keeps the plain `[speaker]` header.
pub fn staged_message_text(
    payload: &serde_json::Value,
    source: &str,
    channels: &shenko_types::channel::ChannelRegistry,
) -> String {
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
    let target = channels
        .channel_for_source(source)
        .and_then(|plugin| (plugin.staged_target)(payload));
    match target {
        Some(target) => format!("[{who} · {target}] {body}"),
        None => format!("[{who}] {body}"),
    }
}

fn stage_input(state: &SubjectLoopState, event: &InboxEvent) -> Result<SubjectHistoryInput, ShenkoError> {
    let mut data = serde_json::json!({"v":1, "inbox_event":event});
    let event_type = match (&event.kind, event.source.as_str()) {
        (InboxEventKind::TaskResult, "native-child") => {
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
        }
        (InboxEventKind::MediaRevision | InboxEventKind::MediaStream, _) => {
            // Typed media facts render through their own honest contract: the
            // stored effective observation and disposition, never the generic
            // message fallback that would paste raw JSON as chat text.
            data["text"] = serde_json::Value::String(staged_media_text(&event.payload)?);
            "user"
        }
        _ => {
            data["text"] = serde_json::Value::String(staged_message_text(
                &event.payload,
                &event.source,
                &state.channels,
            ));
            // Untrusted ingress kinds never acquire system/assistant authority.
            "user"
        }
    };
    // Every staged input cites the durable event identity it came from, so a
    // control or reply can bind to the exact fact (SPEC §41.5) and two
    // same-source deliveries can be told apart in the model history.
    let text = data["text"].as_str().unwrap_or_default().to_owned();
    data["text"] = serde_json::Value::String(format!("[event {}] {text}", event.event_id));
    Ok(SubjectHistoryInput { event_id: event.event_id, event_type: event_type.into(), data })
}

/// The visible text one durable media ingress event contributes to the model
/// history. Revisions replace the segment's effective text explicitly:
/// superseded/correcting revisions say what they replaced or what stayed
/// effective instead of reading as a second contradictory utterance.
/// Speaker claims keep their confidence and stay provider claims — they never
/// acquire user or admin authority. No text is fabricated: every quoted line
/// is the provider's own committed revision.
fn staged_media_text(payload: &serde_json::Value) -> Result<String, ShenkoError> {
    let event: shenko_types::media::MediaIngressEvent =
        serde_json::from_value(payload.clone()).map_err(ShenkoError::Serde)?;
    Ok(match event {
        shenko_types::media::MediaIngressEvent::StreamOpened {
            stream, descriptor, ..
        } => {
            let label = stream_label(&stream);
            if descriptor.is_empty() {
                format!("[媒体流 {label} 开始]")
            } else {
                format!(
                    "[媒体流 {label} 开始] {}",
                    serde_json::to_string(&descriptor).map_err(ShenkoError::Serde)?
                )
            }
        }
        shenko_types::media::MediaIngressEvent::StreamState {
            stream, state, reason, segments, ..
        } => {
            let label = stream_label(&stream);
            let state_text = match state {
                shenko_types::media::MediaStreamState::Cancelled => "已取消",
                shenko_types::media::MediaStreamState::Disconnected => "连接已断开",
                shenko_types::media::MediaStreamState::Ended => "已结束",
                shenko_types::media::MediaStreamState::Open => "已重新打开",
            };
            let reason = reason
                .as_deref()
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
                .map(|reason| format!("：{reason}"))
                .unwrap_or_default();
            format!(
                "[媒体流 {label} {state_text}{reason}；已提交 {segments} 个片段，未到达的片段保持空缺]"
            )
        }
        shenko_types::media::MediaIngressEvent::Revision {
            stream, record, prior_effective, ..
        } => {
            let label = stream_label(&stream);
            let timing = record
                .captured_at_ms
                .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms))
                .map(|time| format!(" 采样于{}", time.to_rfc3339()))
                .unwrap_or_default();
            let speaker = record
                .speaker
                .as_ref()
                .map(|claim| speaker_claim_text(claim))
                .unwrap_or_default();
            match record.disposition {
                shenko_types::media::MediaRevisionDisposition::Superseded => {
                    let current = prior_effective
                        .as_ref()
                        .map(|prior| {
                            format!(
                                "当前有效文本仍为第{}版：{}",
                                prior.revision, prior.text
                            )
                        })
                        .unwrap_or_else(|| "当前有效文本未返回".to_string());
                    format!(
                        "[媒体 {label} 片段{} 第{}版·迟到修订（不生效）{timing}{speaker}] {}（{current}；本条保留为原始记录）",
                        record.segment, record.revision, record.text
                    )
                }
                shenko_types::media::MediaRevisionDisposition::FinalCorrection => {
                    let prior_text = prior_effective
                        .as_ref()
                        .map(|prior| prior.text.clone())
                        .unwrap_or_default();
                    format!(
                        "[媒体 {label} 片段{} 第{}版·对已终稿文本的更正{timing}{speaker}] {}（原终稿第{}版：{}）",
                        record.segment,
                        record.revision,
                        record.text,
                        prior_effective.as_ref().map(|prior| prior.revision).unwrap_or(0),
                        prior_text
                    )
                }
                shenko_types::media::MediaRevisionDisposition::Applied
                | shenko_types::media::MediaRevisionDisposition::AppliedSupersedes => {
                    let state = if record.partial { "未定稿" } else { "定稿" };
                    let note = match (
                        record.disposition,
                        prior_effective.as_ref().map(|prior| prior.revision),
                    ) {
                        (
                            shenko_types::media::MediaRevisionDisposition::AppliedSupersedes,
                            Some(prior_revision),
                        ) => format!("，替换第{prior_revision}版"),
                        _ => String::new(),
                    };
                    format!(
                        "[媒体 {label} 片段{} 第{}版·{state}{note}{timing}{speaker}] {}",
                        record.segment, record.revision, record.text
                    )
                }
            }
        }
    })
}

fn stream_label(stream: &shenko_types::media::MediaStreamDescriptor) -> String {
    format!("{}:{}/{}", stream.provider, stream.account, stream.stream_id)
}

/// One provider speaker claim rendered honestly: tag, confidence, correction
/// and unknown markers, always marked as the provider's claim.
fn speaker_claim_text(claim: &shenko_types::media::MediaSpeakerClaim) -> String {
    let tag = claim
        .speaker
        .as_deref()
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .unwrap_or("未知说话人");
    let confidence = claim
        .confidence
        .map(|value| format!(" 置信{:.2}", value))
        .unwrap_or_default();
    let corrected = if claim.corrected { "·更正" } else { "" };
    let person = claim
        .person_id
        .map(|id| format!(" 声称联系人{id}"))
        .unwrap_or_default();
    let source = claim
        .source
        .as_deref()
        .map(|source| format!(" 由{source}声称"))
        .unwrap_or_else(|| " 由提供者声称".to_string());
    format!("〔{tag}{confidence}{corrected}{person}{source}，非身份授权〕")
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
    if let Some(wait) = state.semantic_wait.as_ref().filter(|wait| wait.is_active()) {
        let binding = serde_json::to_value(state.broker.as_ref().map(|broker| broker.binding()))?;
        let digest = Digest::blake3(&serde_json::to_vec(&binding)?);
        if digest != wait.model_binding_digest {
            return Err(ShenkoError::conflict(
                "semantic wait model binding changed; preserve the wait and reconcile the real provider before continuing",
            ));
        }
    }
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
    // Cognition units are installed material, not a keyword lookup: the model
    // receives every unit's short card (bounded) and decides what, if
    // anything, it means here. A word-matching gate would withhold the unit
    // precisely when the message names a symptom instead of the concept.
    // Unit bodies never travel whole; the model reads the body it actually
    // wants at an exact version through cognition.unit.read.
    let awareness: Vec<String> = state
        .awareness
        .as_ref()
        .map(|catalog| catalog.disclosure().map(|text| vec![text]))
        .transpose()
        .map_err(|error| {
            ShenkoError::storage(format!("cognition disclosure failed: {error:#}"))
        })?
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
    // Record pressure every S0 so the durable window ledger sees the used/
    // available shape even when no strategy fires this step.
    if let Some(caps) = caps.as_ref() {
        registry
            .observe_pressure(caps, trigger_budget_input_tokens)
            .map_err(storage)?;
    }
    let selection = match &caps {
        None => ContextSelection::Unavailable { reason: "model_context_limit_not_observed".into() },
        Some(caps) => {
            // The trigger is a cheap decision over the committed window and
            // the observed pressure; it reads no raw history, so it can run
            // inside an S0 seal.
            if !registry.needs_preparation(caps, trigger_budget_input_tokens) {
                ContextSelection::Retained
            } else {
                // The slow strategy work (history render, policy attempts,
                // window cut) happens OFF the S0 path: a prior step's
                // preparer published a candidate, or this step launches one
                // and keeps the current window. Adoption re-verifies scope,
                // source window, model generation, policy epoch and leaf
                // against this exact bounded history.
                match registry.adopt_prepared(&history).map_err(adopt_error)? {
                    Some((adopted, attempts)) => {
                        window = adopted;
                        (payload, estimated_input_tokens, unfinished_call_ids, source_commit_seq) = project_payload_stable(
                            &state.store, &window, &history, system_prompt, Some(caps), state.max_tokens, &input_raw_event_ids,
                        )?;
                        selected_budget_input_tokens = estimated_input_tokens
                            .ok_or_else(|| ShenkoError::conflict("compacted projection has no trustworthy token estimate"))?;
                        budget_provenance.push_str("; selected compacted heuristic");
                        ContextSelection::Compacted { attempts }
                    }
                    None => {
                        launch_context_preparer(
                            &registry,
                            Arc::clone(&state.store),
                            scope_id,
                            caps.clone(),
                            trigger_budget_input_tokens,
                        );
                        ContextSelection::Preparing { attempts: Vec::new() }
                    }
                }
            }
        }
    };
    let final_estimate = estimated_input_tokens
        .ok_or_else(|| ShenkoError::conflict("selected projection has no trustworthy token estimate"))?;
    // The prepared contributions are collected BEFORE the final budget
    // check, so their cost is charged together with the context and the
    // tool cost (SPEC §37.7) and can never be added after the check.
    let collected = collect_contributions(state, scope_id)?;
    let (contributions, contribution_omissions, contribution_tokens, contribution_plan) =
        match &collected {
        Some(collected) => {
            // The model-facing material of a contribution is the text of the
            // sealed document its reference points at. Its provenance
            // (producer, version, artifact digest) is NOT put in front of
            // the model: that reaches the raw call record through the seal.
            if !collected.material.is_empty() {
                payload.messages.push(InferenceMessage::text(
                    "user",
                    format!(
                        "[prepared contributions]\n{}",
                        collected.material.join("\n")
                    ),
                ));
                estimated_input_tokens =
                    estimate_payload_tokens(&payload, 0).or(estimated_input_tokens);
            }
            (
                collected.contributions.clone(),
                collected.omissions.clone(),
                collected.total_tokens,
                Some(collected.plan.clone()),
            )
        }
        None => (Vec::new(), Vec::new(), 0, None),
    };
    selected_budget_input_tokens = selected_budget_input_tokens.saturating_add(
        u32::try_from(contribution_tokens).unwrap_or(u32::MAX),
    );
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
        contributions,
        contribution_omissions,
        contribution_tokens,
        contribution_plan,
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
