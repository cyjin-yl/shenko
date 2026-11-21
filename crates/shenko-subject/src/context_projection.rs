//! The S0 boundary: durable raw facts -> committed window -> native payload.
use std::{collections::HashMap, sync::Arc};

use futures::FutureExt;
use serde::{Deserialize, Serialize};
use shenko_broker::request::{InferenceMessage, InferencePart, InferencePayload, ProviderCapabilities};
use shenko_context_policy::{
    Attempt, CONTRACT_VERSION, CallStatus, CommittedWindow, HistoryItem, HistorySource, ItemKind, ItemRef, ModelCaps,
    Projected, StorageError, StoreHistory, VisionCaps, estimate_tokens,
};
use shenko_store::{Store, SubjectContextRef, SubjectHistoryInput};
use shenko_types::{error::ShenkoError, ids::EntityId, object::Digest, task::TaskResult};

use shenko_types::inbox::{InboxEvent, InboxEventKind};
use crate::loop_runner::{SubjectLoopState, build_system_prompt, stable_system_prompt};

#[path = "subject_attachment.rs"]
mod subject_attachment;

/// The user turn the product adds when a step has no external event. It keeps
/// the thought loop going without inventing an instruction, and behavior
/// evaluation samples this exact sentence.
pub const IDLE_CONTINUATION_PROMPT: &str = "[运行时连续思考提示；不是新外部消息] 继续。";

/// Project advertised limits into the input budget left after output is reserved.
/// Unknown provider context stays unknown; callers reject an exhausted budget.
pub fn input_model_caps(
    capabilities: &ProviderCapabilities,
    reserved_output_tokens: u32,
) -> Option<ModelCaps> {
    capabilities.max_context_tokens.filter(|limit| *limit > 0).map(|max_context| ModelCaps {
        model_id: capabilities.binding.model().into(),
        max_context: max_context.saturating_sub(reserved_output_tokens),
        vision: capabilities.vision.map(|vision| VisionCaps {
            max_image_context: vision.max_image_context.saturating_sub(reserved_output_tokens),
            patch_px: vision.patch_px,
        }),
    })
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(super) enum ContextSelection {
    Unavailable { reason: String },
    Retained,
    /// Compaction is preparing off the S0 path. A fitting step keeps the
    /// current window; an oversized step waits before sealing any S0.
    Preparing { attempts: Vec<Attempt> },
    NoUsablePolicy { attempts: Vec<Attempt> },
    Compacted { attempts: Vec<Attempt> },
}

/// Exact partitions of the sealed payload. Counts describe rendered messages,
/// never raw-history sequence numbers. Only the history partition may be
/// replaced by a policy; the original idle/contribution text and correction
/// tail remain verbatim.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct FrozenPayloadLayout {
    pub system_prefix_bytes: u64,
    pub history_message_count: u64,
    pub idle_message_count: u64,
    pub contribution_message_count: u64,
    pub vision_tokens: u32,
}

impl FrozenPayloadLayout {
    pub(super) fn partitions(
        &self,
        payload: &InferencePayload,
    ) -> Result<(usize, usize, usize), ShenkoError> {
        let prefix = usize::try_from(self.system_prefix_bytes)
            .map_err(|_| ShenkoError::conflict("frozen system prefix length overflow"))?;
        let history = usize::try_from(self.history_message_count)
            .map_err(|_| ShenkoError::conflict("frozen history message count overflow"))?;
        let contribution_start = self
            .history_message_count
            .checked_add(self.idle_message_count)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| ShenkoError::conflict("frozen retained message count overflow"))?;
        let tail_start = contribution_start
            .checked_add(
                usize::try_from(self.contribution_message_count).map_err(|_| {
                    ShenkoError::conflict("frozen contribution message count overflow")
                })?,
            )
            .ok_or_else(|| ShenkoError::conflict("frozen correction tail offset overflow"))?;
        if self.idle_message_count > 1
            || prefix > payload.system_prompt.len()
            || !payload.system_prompt.is_char_boundary(prefix)
            || tail_start > payload.messages.len()
            || payload.messages[history..]
                .iter()
                .flat_map(|message| &message.parts)
                .any(|part| !matches!(part, InferencePart::Text { .. }))
        {
            return Err(ShenkoError::conflict("invalid frozen payload partitions"));
        }
        let has_image = payload.messages[..history]
            .iter()
            .flat_map(|message| &message.parts)
            .any(InferencePart::is_image);
        if has_image != (self.vision_tokens > 0) {
            return Err(ShenkoError::conflict(
                "frozen payload vision charge differs from its image partition",
            ));
        }
        Ok((history, contribution_start, tail_start))
    }
}

/// A correction is a new request. A policy may make a new observation of
/// execution facts within the original raw-history boundary, but that is
/// explicitly separate from what the parent seal observed.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct FrozenCorrectionObservation {
    pub parent_input_ref: shenko_types::object::ObjectRef,
    pub parent_source: SubjectContextRef,
    pub new_execution_projection: Option<SubjectContextRef>,
    pub tail_digest: Digest,
    pub tail_message_count: u64,
}

#[derive(Clone, Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_layout: Option<FrozenPayloadLayout>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correction_observation: Option<FrozenCorrectionObservation>,
}

pub(super) struct PreparedContextInput {
    pub obligations: Vec<shenko_types::subject::PendingObligation>,
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
}

fn storage(error: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::storage(error.to_string())
}

/// What the contribution registry returned for this step.
pub(super) struct CollectedContributions {
    pub plan: shenko_context_policy::ContributionPlan,
    /// One model-facing part, resolved from the verified contribution bodies.
    pub material: Option<String>,
}

/// Collect the prepared contributions for this step.
///
/// The registry merges them: identical material collapses, a disagreement is
/// reported, and a producer's failure never throws. An omission carries its
/// own reason and is DATA, not a failure of the step. A missing REQUIRED
/// contribution is the decision point: the step is not admissible without
/// it, so it fails with a typed conflict that names exactly what was
/// missing — never a silent drop.
pub(super) fn collect_contributions(
    store: &Store,
    registry: Option<&shenko_context_policy::ContributionRegistry>,
    scope: &shenko_context_policy::ContributionScope,
) -> Result<Option<CollectedContributions>, ShenkoError> {
    let Some(registry) = registry else {
        // No contribution runtime installed: there is nothing to collect.
        return Ok(None);
    };
    let plan = registry.collect(scope, chrono::Utc::now().timestamp_millis());
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
    let mut material = None::<String>;
    for contribution in &plan.contributions {
        let bytes = store.cas.read_object(&contribution.data.digest)?;
        if bytes.len() as u64 != contribution.data.bytes
            || Digest::blake3(&bytes) != contribution.data.digest
        {
            return Err(ShenkoError::ObjectClosureIncomplete { missing_count: 1 });
        }
        let text = shenko_context_policy::contribution::ContributionPayload::read(&bytes)
            .map_err(|error| ShenkoError::storage(format!("CONTRIBUTION_PAYLOAD_INVALID: {error}")))?;
        match &mut material {
            Some(material) => { material.push('\n'); material.push_str(&text); }
            None => { material = Some(format!("[prepared contributions]\n{text}")); }
        }
    }
    Ok(Some(CollectedContributions { plan, material }))
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

#[derive(PartialEq, Eq)]
struct PreparationSource {
    window_id: EntityId,
    model_generation: u64,
    leaf: Option<u64>,
    configuration_revision: u64,
    used_tokens: u32,
    caps: ModelCaps,
}

struct PreparedObservation {
    candidate: shenko_context_policy::PreparedCandidate,
    baseline: PreparationBaseline,
}

struct PreparationBaseline {
    source_commit_seq: u64,
    leaf: Option<u64>,
    calls: HashMap<EntityId, (ItemRef, CallStatus)>,
}

enum PreparationFailure {
    Policy(shenko_context_policy::PrepareError),
    ObservationMoved { before: u64, after: u64 },
}

type PreparationResult =
    Result<Result<PreparedObservation, PreparationFailure>, tokio::task::JoinError>;

/// One owned worker. Only the live subject publishes its result; aborting a
/// running blocking task cannot preempt strategy code, but dropping its owner
/// cannot leave a worker that later publishes into the registry.
pub(super) struct ContextPreparer {
    source: PreparationSource,
    task: tokio::task::JoinHandle<Result<PreparedObservation, PreparationFailure>>,
    completed: Option<PreparationResult>,
}

impl Drop for ContextPreparer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn launch_context_preparer(
    registry: &Arc<shenko_context_policy::PolicyRegistry>,
    store: Arc<Store>,
    scope_id: EntityId,
    caps: ModelCaps,
    source: PreparationSource,
    cache: Arc<shenko_context_policy::HistoryProjectionCache>,
) -> ContextPreparer {
    let registry = Arc::clone(registry);
    let used_tokens = source.used_tokens;
    let task = tokio::task::spawn_blocking(move || {
        let policy_error = |error| {
            PreparationFailure::Policy(shenko_context_policy::PrepareError::Storage(
                storage_message(error),
            ))
        };
        let before = store.current_commit_seq().map_err(policy_error)?;
        let history = StoreHistory::with_cache(Arc::clone(&store), scope_id, Arc::new(estimate_tokens), cache);
        // Build the call metadata in the owned background worker before the
        // first adoption needs it on S0. Later workers advance only its tail.
        if let Some(leaf) = source.leaf {
            history.call_items(leaf).map_err(|error| PreparationFailure::Policy(
                shenko_context_policy::PrepareError::Storage(error)))?;
        }
        let observed = RecordingHistory {
            history: BoundedHistory {
                history: &history,
                leaf: source.leaf,
            },
            calls: parking_lot::Mutex::new(HashMap::new()),
        };
        let candidate = registry.prepare_candidate(&observed, &caps, used_tokens);
        let after = store.current_commit_seq().map_err(policy_error)?;
        if before != after {
            return Err(PreparationFailure::ObservationMoved { before, after });
        }
        tracing::debug!(source_commit_seq = before, leaf = ?source.leaf,
            call_observations = observed.calls.lock().len(),
            "context policy prepared against a stable execution observation");
        Ok(PreparedObservation {
            candidate: candidate.map_err(PreparationFailure::Policy)?,
            baseline: PreparationBaseline {
                source_commit_seq: before,
                leaf: source.leaf,
                calls: observed.calls.into_inner(),
            },
        })
    });
    ContextPreparer {
        source,
        task,
        completed: None,
    }
}

/// Wait without an inference permit or S0 seal. Keep completion until the
/// next projection, which rechecks the live source before admitting it.
pub(super) async fn wait_for_context_preparer(
    state: &mut SubjectLoopState,
    cancel: &tokio_util::sync::CancellationToken,
) {
    let Some(preparer) = state.context_preparer.as_mut() else {
        return;
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => { state.context_preparer.take(); }
        result = &mut preparer.task => { preparer.completed = Some(result); }
    }
}

/// Bound the append-only source before selecting/projecting a window. A later
/// append is rejected by the final Store S0 CAS, never silently added here.
struct BoundedHistory<'a> {
    history: &'a StoreHistory,
    leaf: Option<u64>,
}

impl HistorySource for BoundedHistory<'_> {
    fn call_items(&self, through: u64) -> Result<Vec<HistoryItem>, StorageError> {
        match self.leaf {
            Some(leaf) => self.history.call_items(through.min(leaf)),
            None => Ok(Vec::new()),
        }
    }

    fn leaf_seq(&self) -> Result<Option<u64>, StorageError> {
        Ok(self.leaf)
    }

    fn read_range(&self, from: u64, to: u64) -> Result<Vec<HistoryItem>, StorageError> {
        match self.leaf {
            Some(leaf) if from <= leaf => self.history.read_range(from, to.min(leaf)),
            _ => Ok(Vec::new()),
        }
    }

    fn read_event(&self, id: EntityId) -> Result<Option<HistoryItem>, StorageError> {
        Ok(self
            .history
            .read_event(id)?
            .filter(|item| self.leaf.is_some_and(|leaf| item.at.seq <= leaf)))
    }
}

fn storage_message(error: impl std::fmt::Display) -> StorageError {
    StorageError(error.to_string())
}

// The policy already reads its required history. Retain only the mutable
// execution states it actually saw, not another unbounded copy of that text.
// Refuse this preparation before admitting more than this bounded index.
const MAX_PREPARATION_CALL_OBSERVATIONS: usize = 65_536;

struct RecordingHistory<'a> {
    history: BoundedHistory<'a>,
    calls: parking_lot::Mutex<HashMap<EntityId, (ItemRef, CallStatus)>>,
}

impl RecordingHistory<'_> {
    fn observe(&self, item: &HistoryItem) -> Result<(), StorageError> {
        let status = match &item.kind {
            ItemKind::ToolCall { status, .. } | ItemKind::ToolResult { status, .. } => *status,
            _ => return Ok(()),
        };
        let mut calls = self.calls.lock();
        match calls.get(&item.at.event_id) {
            Some(previous) if *previous != (item.at, status) => {
                return Err(storage_message(
                    "execution state changed during policy preparation",
                ));
            }
            Some(_) => {}
            None if calls.len() >= MAX_PREPARATION_CALL_OBSERVATIONS => {
                return Err(storage_message(
                    "policy preparation exceeds the bounded execution-observation index",
                ));
            }
            None => {
                calls.insert(item.at.event_id, (item.at, status));
            }
        }
        Ok(())
    }
}

impl HistorySource for RecordingHistory<'_> {
    fn call_items(&self, through: u64) -> Result<Vec<HistoryItem>, StorageError> {
        let items = self.history.call_items(through)?;
        for item in &items { self.observe(item)?; }
        Ok(items)
    }
    fn leaf_seq(&self) -> Result<Option<u64>, StorageError> {
        self.history.leaf_seq()
    }
    fn read_range(&self, from: u64, to: u64) -> Result<Vec<HistoryItem>, StorageError> {
        let items = self.history.read_range(from, to)?;
        for item in &items {
            self.observe(item)?;
        }
        Ok(items)
    }
    fn read_event(&self, id: EntityId) -> Result<Option<HistoryItem>, StorageError> {
        let item = self.history.read_event(id)?;
        if let Some(item) = &item {
            self.observe(item)?;
        }
        Ok(item)
    }
}

/// Adoption uses the worker's execution baseline for the prepared prefix.
/// Later raw events stay live. The final projection uses the live history and
/// the committed baseline to rehydrate any execution states that changed.
struct AdoptionHistory<'a, 'store> {
    history: &'a BoundedHistory<'store>,
    prepared: &'a PreparationBaseline,
}

impl AdoptionHistory<'_, '_> {
    fn restore(&self, item: &mut HistoryItem) -> Result<(), StorageError> {
        if self.prepared.leaf.is_none_or(|leaf| item.at.seq > leaf) {
            return Ok(());
        }
        match &mut item.kind {
            ItemKind::ToolCall { status, .. } | ItemKind::ToolResult { status, .. } => {
                let (at, observed) =
                    self.prepared.calls.get(&item.at.event_id).ok_or_else(|| {
                        storage_message("prepared prefix lacks its execution observation")
                    })?;
                if at != &item.at {
                    return Err(storage_message("prepared raw event identity changed"));
                }
                *status = *observed;
            }
            _ => {}
        }
        Ok(())
    }
}

impl HistorySource for AdoptionHistory<'_, '_> {
    fn call_items(&self, through: u64) -> Result<Vec<HistoryItem>, StorageError> {
        let mut items = self.history.call_items(through)?;
        for item in &mut items { self.restore(item)?; }
        Ok(items)
    }
    fn leaf_seq(&self) -> Result<Option<u64>, StorageError> {
        self.history.leaf_seq()
    }
    fn read_range(&self, from: u64, to: u64) -> Result<Vec<HistoryItem>, StorageError> {
        let mut items = self.history.read_range(from, to)?;
        for item in &mut items {
            self.restore(item)?;
        }
        Ok(items)
    }
    fn read_event(&self, id: EntityId) -> Result<Option<HistoryItem>, StorageError> {
        let mut item = self.history.read_event(id)?;
        if let Some(item) = &mut item {
            self.restore(item)?;
        }
        Ok(item)
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
    // The owning plugin classifies the payload and renders the model's view
    // of it. A payload that is already the normalized envelope classifies to
    // itself, so the projection is stable across admission and replay.
    if let Some(plugin) = channels.channel_for_source(source) {
        if let Some(envelope) = plugin.classify(source, payload) {
            return plugin.staged_text(&envelope);
        }
    }
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
    format!("[{who}] {body}")
}

fn stage_input(state: &SubjectLoopState, event: &InboxEvent) -> Result<SubjectHistoryInput, ShenkoError> {
    // A deferred channel message may enter a later step after a projector
    // upgrade. Keep the first exact source original AND its visible text;
    // missing historical attribution must not be reinterpreted by new code.
    if event.kind == InboxEventKind::Message
        && state.channels.channel_for_source(&event.source).is_some()
        && let Some(staged) = state.store.first_subject_input(state.agent_id, event.event_id)?
        && same_source_delivery(&staged_original(&staged.data)?, event)
    {
        return Ok(SubjectHistoryInput {
            event_id: event.event_id,
            event_type: staged.event_type,
            data: serde_json::from_str(&staged.data)?,
        });
    }
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
        (
            InboxEventKind::MediaRevision
            | InboxEventKind::MediaStream
            | InboxEventKind::MediaRecording
            | InboxEventKind::MediaPriority,
            _,
        ) => {
            // Typed media facts render through their own honest contract: the
            // stored effective observation and disposition, never the generic
            // message fallback that would paste raw JSON as chat text.
            data["text"] = serde_json::Value::String(staged_media_text(&event.payload)?);
            "user"
        }
        _ => {
            let web = event.source == shenko_store::web_input::WEB_INPUT_SOURCE;
            let attachments_only = web
                && event.payload.get("content").and_then(serde_json::Value::as_str).is_some_and(|content| content.trim().is_empty());
            // An attachment-only message has no body to quote; the generic
            // fallback would otherwise paste the raw payload.
            let mut text = if attachments_only {
                format!("[{}]", event.source)
            } else {
                staged_message_text(&event.payload, &event.source, &state.channels)
            };
            if web {
                text.push_str(&web_attachment_description(&state.store, &event.payload,
                    "（图片；是否传递画面由本轮封存请求的视觉能力与栅格验证决定）")?);
            }
            data["text"] = serde_json::Value::String(text);
            // Untrusted ingress kinds never acquire system/assistant authority.
            "user"
        }
    };
    // Every staged input cites the durable event identity it came from, so a
    // control or reply can bind to the exact fact and two
    // same-source deliveries can be told apart in the model history.
    let text = data["text"].as_str().unwrap_or_default().to_owned();
    data["text"] = serde_json::Value::String(format!("[event {}] {text}", event.event_id));
    Ok(SubjectHistoryInput { event_id: event.event_id, event_type: event_type.into(), data })
}

/// Bytes of one text attachment shown inline; the rest is reported, not shown.
const INLINE_ATTACHMENT_BYTES: usize = 24 * 1024;

/// What the subject sees of a browser submission's attachments: each one's
/// name, type, size and digest, a text attachment's leading text, and for an
/// image the fact that this inference path carries no image input. Nothing is
/// described that was not read: an image's content is not guessed.
pub fn web_attachments_text(store: &shenko_store::store::Store, payload: &serde_json::Value) -> Result<String, ShenkoError> {
    web_attachment_description(store, payload, "（图片；当前推理路径不传递图像输入，看不到画面）")
}

fn web_attachment_description(
    store: &Store,
    payload: &serde_json::Value,
    image_notice: &str,
) -> Result<String, ShenkoError> {
    use shenko_store::web_attachment::WebAttachment;
    let Some(references) = payload.get("attachment_refs").and_then(serde_json::Value::as_array) else {
        return Ok(String::new());
    };
    let mut text = String::new();
    for (index, reference) in references.iter().enumerate() {
        let attachment: WebAttachment = serde_json::from_value(reference.clone()).map_err(ShenkoError::Serde)?;
        text.push_str(&format!(
            "\n[附件 {}] {} · {} · {} 字节 · {}",
            index + 1,
            attachment.name,
            attachment.media_type,
            attachment.bytes,
            attachment.digest.0
        ));
        if attachment.is_text() {
            let prefix = store.web_attachment_prefix(&attachment, INLINE_ATTACHMENT_BYTES)?;
            let shown = match std::str::from_utf8(&prefix) {
                Ok(all) => all,
                // A cut inside a multi-byte character keeps the valid part.
                Err(error) if error.error_len().is_none() => std::str::from_utf8(&prefix[..error.valid_up_to()]).unwrap_or(""),
                Err(_) => {
                    text.push_str("\n（声明为文本，但内容不是 UTF-8，未内联）");
                    continue;
                }
            };
            text.push_str(&format!("\n```\n{shown}\n```"));
            let rest = attachment.bytes.saturating_sub(shown.len() as u64);
            if rest > 0 {
                text.push_str(&format!("\n（其后还有 {rest} 字节未内联）"));
            }
        } else if attachment.is_image() {
            text.push('\n');
            text.push_str(image_notice);
        } else {
            text.push_str("\n（非文本文件，内容未内联）");
        }
    }
    Ok(text)
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
        // ArchMedia: recording controls and priority inputs render as
        // ordinary durable facts of the same event model. ArchSubject
        // owns this file; these arms are additive and mirror the style above.
        shenko_types::media::MediaIngressEvent::Recording {
            stream, recording, action, ..
        } => {
            let label = stream_label(&stream);
            let action_text = match action {
                shenko_types::media::MediaRecordingAction::Start => "开始",
                shenko_types::media::MediaRecordingAction::Stop => "停止",
            };
            let retention = if recording.scope.retention.is_empty() {
                String::new()
            } else {
                format!(" 保留{}", recording.scope.retention)
            };
            format!(
                "[媒体 {label} 录音{}已{action_text}{retention}；状态{}]",
                recording.recording_id,
                match recording.state {
                    shenko_types::media::MediaRecordingState::Recording => "录音中",
                    shenko_types::media::MediaRecordingState::Stopped => "已停止",
                    shenko_types::media::MediaRecordingState::Failed => "失败",
                },
            )
        }
        shenko_types::media::MediaIngressEvent::Priority { stream, input, .. } => {
            let label = stream_label(&stream);
            let kind_text = match input.kind {
                shenko_types::media::MediaPriorityKind::BargeIn => "打断",
                shenko_types::media::MediaPriorityKind::Interjection => "插话",
                shenko_types::media::MediaPriorityKind::StopPlayback => "停止播音",
                shenko_types::media::MediaPriorityKind::UrgentMessage => "紧急消息",
            };
            let text = input
                .text
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(|text| format!("：{text}"))
                .unwrap_or_default();
            let reason = input
                .reason
                .as_deref()
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
                .map(|reason| format!("（{reason}）"))
                .unwrap_or_default();
            format!("[媒体 {label} 优先输入·{kind_text}{text}{reason}]")
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
            InferencePart::Document { .. }
            | InferencePart::Citation { .. }
            | InferencePart::SearchResult { .. } => {}
        }
    }
    if has_image {
        (vision_tokens > 0).then(|| estimate.saturating_add(vision_tokens))
    } else {
        Some(estimate)
    }
}

pub(super) struct PayloadProjectionOptions<'a> {
    pub(super) caps: Option<&'a ModelCaps>,
    pub(super) max_tokens: u64,
    pub(super) idle_text: Option<&'a str>,
}

struct ProjectedPayload {
    payload: InferencePayload,
    estimated_input_tokens: Option<u32>,
    unfinished_call_ids: Vec<String>,
    payload_layout: FrozenPayloadLayout,
}

fn project_payload(
    store: &Store,
    scope: EntityId,
    window: &CommittedWindow,
    history: &dyn HistorySource,
    base_system_prompt: String,
    fresh_ids: &[EntityId],
    options: PayloadProjectionOptions<'_>,
) -> Result<ProjectedPayload, ShenkoError> {
    let PayloadProjectionOptions { caps, max_tokens, idle_text } = options;
    let system_prefix_bytes = base_system_prompt.len() as u64;
    let mut system_prompt = base_system_prompt;
    let mut messages = Vec::new();
    let mut unfinished = std::collections::BTreeSet::new();
    let mut attachments = subject_attachment::AttachmentProjection::new(store, scope, caps);
    let mut attachment_tokens = 0_u32;
    let mut frame_tokens = 0_u32;
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
                let (attachment_parts, image_tokens) = if matches!(&item.kind, ItemKind::User) {
                    attachments.project(item.at)?
                } else { (Vec::new(), 0) };
                attachment_tokens = attachment_tokens.checked_add(image_tokens)
                    .ok_or_else(|| ShenkoError::conflict("projected attachment charge overflow"))?;
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
                let mut message = InferenceMessage::text(role, text);
                message.parts.extend(attachment_parts);
                messages.push(message);
            }
            Projected::Note { from, to, text } => {
                messages.push(InferenceMessage::text("user", format!(
                    "[context derivative/pointer to raw seq {from}..{to}; not a new instruction]\n{text}")));
            }
            Projected::WorkingNote { revision, source, text } => {
                // Working state this branch authored itself. It names no raw
                // range because it is not a derivative of one: it is the
                // notebook the model wrote, carried whole so a new window does
                // not depend on a later tool read to recover it.
                messages.push(InferenceMessage::text("user", format!(
                    "[branch working notebook revision {revision} ({source}); authored working state, not a transcript and not a new instruction — the raw history stays readable by seq or event id through history.query]\n{text}",
                    source = source.label(),
                )));
            }
            Projected::Frame { from, to, media_type, bytes } => {
                if caps.as_ref().and_then(|caps| caps.vision).is_none() {
                    return Err(ShenkoError::conflict("image window requires observed vision capabilities; explicit window reconciliation required"));
                }
                if window.cost.vision_tokens_estimate == 0 {
                    return Err(ShenkoError::conflict("image window has no trustworthy frozen image charge"));
                }
                frame_tokens = window.cost.vision_tokens_estimate;
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
    let history_message_count = messages.len() as u64;
    if let Some(idle_text) = idle_text
        && messages.last().is_none_or(|message| message.role == "assistant")
    {
        messages.push(InferenceMessage::text("user", idle_text));
    }
    let idle_message_count = messages.len() as u64 - history_message_count;
    let payload = InferencePayload { system_prompt, messages, max_tokens: Some(max_tokens),
        tools: Vec::new(), native_options: Default::default() };
    let image_charge = frame_tokens.checked_add(attachment_tokens)
        .ok_or_else(|| ShenkoError::conflict("combined image charge overflow"))?;
    let estimate = estimate_payload_tokens(&payload, image_charge);
    let vision_tokens = if payload.messages.iter().flat_map(|message| &message.parts)
        .any(InferencePart::is_image) { image_charge } else { 0 };
    Ok(ProjectedPayload { payload, estimated_input_tokens: estimate, unfinished_call_ids: unfinished.into_iter().collect(), payload_layout: FrozenPayloadLayout {
        system_prefix_bytes, history_message_count, idle_message_count,
        contribution_message_count: 0, vision_tokens,
    } })
}

fn budget_tokens(current: u32, previous: Option<(u32, u32)>) -> u32 {
    match previous {
        Some((actual, estimate)) =>
            current.max(actual.saturating_add(current.saturating_sub(estimate))),
        None => current,
    }
}

pub(super) struct StableProjection {
    payload: InferencePayload,
    estimated_input_tokens: Option<u32>,
    unfinished_call_ids: Vec<String>,
    source_commit_seq: u64,
    payload_layout: FrozenPayloadLayout,
}

pub(super) fn project_payload_stable(
    store: &Store,
    scope: EntityId,
    window: &CommittedWindow,
    history: &dyn HistorySource,
    base_system_prompt: String,
    fresh_ids: &[EntityId],
    options: PayloadProjectionOptions<'_>,
) -> Result<StableProjection, ShenkoError> {
    // The revision is acquired BEFORE any execution-status read. Equality
    // after projection is what permits this exact revision to enter the seal.
    let source_commit_seq = store.current_commit_seq()?;
    let ProjectedPayload { payload, estimated_input_tokens, unfinished_call_ids, payload_layout } = project_payload(
        store,
        scope,
        window,
        history,
        base_system_prompt,
        fresh_ids,
        options,
    )?;
    if store.current_commit_seq()? != source_commit_seq {
        return Err(ShenkoError::ResnapshotRequired {
            reason: "execution facts changed during projection".into(),
        });
    }
    Ok(StableProjection {
        payload,
        estimated_input_tokens,
        unfinished_call_ids,
        source_commit_seq,
        payload_layout,
    })
}

/// Bind a window to the actual subject configuration without staging input or
/// calling a model. Preparers use the same binding as the next S0; a later
/// configuration change still invalidates their window through the usual rebind.
pub(super) fn bind_context_profile(state: &SubjectLoopState) -> Result<Digest, ShenkoError> {
    let registry = &state.context_registry;
    let binding = state.context_binding();
    if state.context_model_capabilities().as_ref().is_some_and(|caps| Some(&caps.binding) != binding) {
        return Err(ShenkoError::conflict("context capabilities/provider binding mismatch"));
    }
    let profile_digest = Digest::blake3(&serde_json::to_vec(&(
        binding, state.context_model_capabilities(), &stable_system_prompt(state), state.max_tokens,
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
    Ok(profile_digest)
}

struct WindowSelection {
    selection: ContextSelection,
    preparation_error: Option<ShenkoError>,
}

/// Shared owned preparation/adoption. The supplied history is bounded both
/// here and inside the worker. A caller may reserve immutable material outside
/// the policy window by giving it a smaller preparation capacity.
fn select_context_window(
    state: &mut SubjectLoopState,
    history: &BoundedHistory<'_>,
    caps: Option<&ModelCaps>,
    preparation_caps: Option<&ModelCaps>,
    window: &mut CommittedWindow,
    trigger_budget_input_tokens: u32,
) -> Result<WindowSelection, ShenkoError> {
    let Some(caps) = caps else {
        return Ok(WindowSelection {
            selection: ContextSelection::Unavailable {
                reason: "model_context_limit_not_observed".into(),
            },
            preparation_error: None,
        });
    };
    let preparation_caps = preparation_caps.unwrap_or(caps);
    let registry = Arc::clone(&state.context_registry);
    registry
        .observe_pressure(caps, trigger_budget_input_tokens)
        .map_err(storage)?;
    // Keep a completed owned worker until its candidate is needed; publication
    // and adoption below share its exact execution baseline in this call.
    if !registry.needs_preparation(caps, trigger_budget_input_tokens) {
        return Ok(WindowSelection {
            selection: ContextSelection::Retained,
            preparation_error: None,
        });
    }
    let preparation_source = PreparationSource {
        window_id: window.id,
        model_generation: window.model_generation,
        leaf: history.leaf,
        configuration_revision: registry.configuration_revision(),
        used_tokens: trigger_budget_input_tokens,
        caps: preparation_caps.clone(),
    };
    let completed = state.context_preparer.as_mut().and_then(|preparer| {
        preparer
            .completed
            .take()
            .or_else(|| (&mut preparer.task).now_or_never())
    });
    let mut preparation_error = None;
    let mut failed_attempts = Vec::new();
    let mut published_current_source = false;
    let mut prepared_baseline = None;
    if let Some(completed) = completed {
        let preparer = state
            .context_preparer
            .take()
            .ok_or_else(|| ShenkoError::conflict("completed context preparation lost its owner"))?;
        let result = match completed {
            Ok(Ok(prepared)) => {
                let result = registry
                    .publish(prepared.candidate)
                    .map_err(shenko_context_policy::PrepareError::Publish);
                published_current_source = result.is_ok() && preparer.source == preparation_source;
                if result.is_ok() {
                    prepared_baseline = Some(prepared.baseline);
                }
                result
            }
            Ok(Err(PreparationFailure::Policy(error))) => Err(error),
            Ok(Err(PreparationFailure::ObservationMoved { before, after })) => {
                return Err(ShenkoError::ResnapshotRequired {
                    reason: format!(
                        "policy worker observed ledger movement {before}->{after}; no candidate published or request sealed",
                    ),
                });
            }
            Err(error) => {
                tracing::warn!(%error, "context preparer stopped without a candidate");
                if preparer.source == preparation_source {
                    preparation_error = Some(ShenkoError::other(format!(
                        "context preparer failed: {error}"
                    )));
                }
                Ok(())
            }
        };
        if let Err(error) = result {
            tracing::warn!(%error, "context preparer produced no publishable candidate");
            // Stale candidates are not failures of the new projection. A
            // failed old configuration likewise cannot condemn its successor.
            let stale = matches!(
                &error,
                shenko_context_policy::PrepareError::Publish(
                    shenko_context_policy::PublishError::StaleSource { .. }
                        | shenko_context_policy::PublishError::ModelGenerationChanged { .. }
                        | shenko_context_policy::PublishError::StalePolicy { .. }
                )
            );
            if !stale && preparer.source == preparation_source {
                preparation_error = Some(match &error {
                    shenko_context_policy::PrepareError::Storage(error) => storage(error),
                    shenko_context_policy::PrepareError::Publish(
                        shenko_context_policy::PublishError::Storage(error),
                    ) => storage(error),
                    _ => ShenkoError::conflict(format!("context preparation refused: {error}")),
                });
                if let shenko_context_policy::PrepareError::NoUsablePolicy { attempts } = error {
                    failed_attempts = attempts;
                }
            }
        }
    }
    let selection = {
        // The slow strategy work (history render, policy attempts,
        // window cut) happens OFF the S0 path: a prior step's
        // preparer returned a candidate, or this step launches one.
        // A fitting request can keep the current window; otherwise
        // it waits before S0. Adoption re-verifies scope,
        // source window, model generation, policy epoch and leaf
        // against this exact bounded history.
        let adopted = if let Some(prepared) = &prepared_baseline {
            tracing::debug!(
                source_commit_seq = prepared.source_commit_seq,
                "adopting context policy with its worker execution baseline"
            );
            registry.adopt_prepared(&AdoptionHistory { history, prepared })
        } else {
            registry.adopt_prepared(history)
        }
        .map_err(adopt_error)?;
        match adopted {
            Some((adopted, attempts)) => {
                if let Some(prepared) = &prepared_baseline {
                    tracing::info!(worker_source_commit_seq = prepared.source_commit_seq,
                                worker_leaf = ?prepared.leaf, window_id = %adopted.id,
                                "context window adopted with its worker execution observation");
                }
                *window = adopted;
                ContextSelection::Compacted { attempts }
            }
            None => {
                if published_current_source
                    && registry.last_preparation().is_some_and(|diagnostic| {
                        diagnostic.outcome == shenko_context_policy::PreparationOutcome::Refused
                    })
                {
                    preparation_error = Some(ShenkoError::conflict(
                        "prepared context window was refused for the unchanged source",
                    ));
                }
                if preparation_error.is_some() {
                    ContextSelection::NoUsablePolicy {
                        attempts: failed_attempts,
                    }
                } else {
                    if state.context_preparer.is_none() {
                        state.context_preparer = Some(launch_context_preparer(
                            &registry,
                            Arc::clone(&state.store),
                            registry.ledger().scope_id(),
                            preparation_caps.clone(),
                            preparation_source,
                            Arc::clone(&state.context_history_cache),
                        ));
                    }
                    ContextSelection::Preparing {
                        attempts: Vec::new(),
                    }
                }
            }
        }
    };
    Ok(WindowSelection {
        selection,
        preparation_error,
    })
}

fn correction_estimate(payload: &InferencePayload, vision_tokens: u32) -> Result<u32, ShenkoError> {
    if !payload.tools.is_empty()
        || !payload.native_options.is_empty()
        || payload
            .messages
            .iter()
            .flat_map(|message| &message.parts)
            .any(|part| {
                !matches!(
                    part,
                    InferencePart::Text { .. } | InferencePart::Image { .. }
                )
            })
    {
        return Err(ShenkoError::conflict(
            "correction budget cannot count this frozen native/document payload; explicit reconciliation required; no dispatch",
        ));
    }
    estimate_payload_tokens(payload, vision_tokens).ok_or_else(|| {
        ShenkoError::conflict(
            "correction budget has no trustworthy frozen image charge; no dispatch",
        )
    })
}

fn text_message_tokens(messages: &[InferenceMessage]) -> u32 {
    messages
        .iter()
        .flat_map(|message| &message.parts)
        .fold(0_u32, |tokens, part| match part {
            InferencePart::Text { text } => tokens.saturating_add(estimate_tokens(text)),
            _ => tokens,
        })
}

fn correction_limit(caps: Option<&ModelCaps>, payload: &InferencePayload) -> Option<u32> {
    caps.and_then(|caps| {
        if payload
            .messages
            .iter()
            .flat_map(|message| &message.parts)
            .any(InferencePart::is_image)
        {
            caps.image_request_context()
        } else {
            Some(caps.max_context)
        }
    })
}

/// Materialize a policy's new observation within the parent's raw scope/leaf.
/// The caller records this observation separately from the immutable parent
/// seal. No new ingress, persona, contribution, idle text or provider is read.
struct CorrectionProjection<'a> {
    payload: &'a InferencePayload,
    context: &'a FrozenContextInput,
    layout: &'a FrozenPayloadLayout,
}

fn project_correction(
    store: &Store,
    history: &BoundedHistory<'_>,
    window: &CommittedWindow,
    parent: &CorrectionProjection<'_>,
    caps: Option<&ModelCaps>,
    tail: &[InferenceMessage],
) -> Result<
    (
        InferencePayload,
        FrozenPayloadLayout,
        SubjectContextRef,
        Vec<String>,
    ),
    ShenkoError,
> {
    let CorrectionProjection {
        payload: parent_payload,
        context: parent_context,
        layout: parent_layout,
    } = parent;
    let (history_end, _, tail_start) = parent_layout.partitions(parent_payload)?;
    let prefix_end = usize::try_from(parent_layout.system_prefix_bytes)
        .map_err(|_| ShenkoError::conflict("frozen system prefix overflow"))?;
    let source_commit_seq = store.current_commit_seq()?;
    let ProjectedPayload { mut payload, unfinished_call_ids: unfinished, payload_layout: mut layout, .. } = project_payload(
        store,
        parent_context.source.scope_id,
        window,
        history,
        parent_payload.system_prompt[..prefix_end].to_owned(),
        &parent_context.source.input_raw_event_ids,
        PayloadProjectionOptions {
            caps,
            max_tokens: parent_payload.max_tokens.ok_or_else(|| {
                ShenkoError::conflict("correction has no explicit frozen output reservation")
            })?,
            idle_text: None,
        },
    )?;
    if payload.system_prompt != parent_payload.system_prompt {
        return Err(ShenkoError::conflict(
            "correction policy would change frozen system material; explicit reconciliation required; no dispatch",
        ));
    }
    payload
        .messages
        .extend_from_slice(&parent_payload.messages[history_end..tail_start]);
    payload.messages.extend_from_slice(tail);
    payload.max_tokens = parent_payload.max_tokens;
    payload.tools = parent_payload.tools.clone();
    payload.native_options = parent_payload.native_options.clone();
    layout.idle_message_count = parent_layout.idle_message_count;
    layout.contribution_message_count = parent_layout.contribution_message_count;
    layout.partitions(&payload)?;
    if store.current_commit_seq()? != source_commit_seq {
        return Err(ShenkoError::ResnapshotRequired {
            reason: "execution facts changed during correction projection; parent result retained"
                .into(),
        });
    }
    let mut observation = parent_context.source.clone();
    observation.source_commit_seq = source_commit_seq;
    observation.window_id = window.id;
    observation.window_generation = window.generation;
    observation.model_generation = window.model_generation;
    Ok((payload, layout, observation, unfinished))
}

/// Prepare a complete new correction before the R0 replacement transaction.
/// None means the owned policy worker is still needed. The original
/// ResultRecorded step and its terminal receipt remain the only request facts
/// until this returns a fitting payload and Store accepts the replacement.
pub(super) struct CorrectionInput<'a> {
    pub parent_ref: &'a shenko_types::object::ObjectRef,
    pub parent_payload: &'a InferencePayload,
    pub parent_context: &'a FrozenContextInput,
    pub contribution_tokens: u64,
    pub observed_input_tokens: Option<u32>,
    pub raw_output: &'a str,
    pub feedback: String,
}

pub(super) fn prepare_corrected_context(
    state: &mut SubjectLoopState,
    correction: CorrectionInput<'_>,
) -> Result<Option<(InferencePayload, FrozenContextInput)>, ShenkoError> {
    let CorrectionInput {
        parent_ref,
        parent_payload,
        parent_context,
        contribution_tokens,
        observed_input_tokens,
        raw_output,
        feedback,
    } = correction;
    if parent_payload.max_tokens != Some(u64::from(parent_context.reserved_output_tokens)) {
        return Err(ShenkoError::conflict(
            "correction output reservation differs from its frozen payload; no dispatch",
        ));
    }
    let caps = parent_context
        .capabilities
        .as_deref()
        .and_then(|capabilities| {
            input_model_caps(capabilities, parent_context.reserved_output_tokens)
        });
    let mut payload = parent_payload.clone();
    payload
        .messages
        .push(InferenceMessage::text("assistant", raw_output));
    payload
        .messages
        .push(InferenceMessage::text("user", feedback));
    let mut context = parent_context.clone();
    let Some(parent_layout) = parent_context.payload_layout.as_ref() else {
        // Old plans have no trustworthy boundary for their image/contribution
        // charge or policy projection. Plain text can be fully recounted;
        // never extrapolate the stale estimate produced by the old bug.
        if contribution_tokens != 0 {
            return Err(ShenkoError::conflict(
                "legacy correction contribution charge requires explicit layout migration; no dispatch",
            ));
        }
        let previous = correction_estimate(parent_payload, 0)?;
        let estimate = correction_estimate(&payload, 0)?;
        let budget = budget_tokens(estimate, observed_input_tokens.zip(Some(previous)));
        if correction_limit(caps.as_ref(), &payload).is_some_and(|limit| budget > limit) {
            return Err(ShenkoError::conflict(format!(
                "legacy correction input budget {budget} exceeds available context; policy projection requires explicit layout migration; original result retained; no dispatch"
            )));
        }
        context.observed_input_tokens = observed_input_tokens;
        context.estimated_input_tokens = Some(estimate);
        context.budget_input_tokens = Some(budget);
        context.trigger_budget_input_tokens = Some(budget);
        context.budget_provenance = format!(
            "complete legacy text heuristic with immediate parent provider/estimate calibration; reserved output {}; no policy projection identity inferred",
            context.reserved_output_tokens
        );
        context.selection = ContextSelection::Unavailable {
            reason: "legacy_frozen_payload_layout_not_recorded".into(),
        };
        context.correction_observation = None;
        return Ok(Some((payload, context)));
    };
    let (history_end, contribution_start, tail_start) = parent_layout.partitions(parent_payload)?;
    let parent_projection = CorrectionProjection {
        payload: parent_payload,
        context: parent_context,
        layout: parent_layout,
    };
    let tail = payload.messages[tail_start..].to_vec();
    let contribution_estimate =
        text_message_tokens(&parent_payload.messages[contribution_start..tail_start]);
    let declared = u32::try_from(contribution_tokens).map_err(|_| {
        ShenkoError::conflict("frozen contribution charge exceeds token budget type")
    })?;
    let contribution_extra = declared.saturating_sub(contribution_estimate);
    let parent_estimate = correction_estimate(parent_payload, parent_layout.vision_tokens)?;
    let parent_charged = parent_estimate.saturating_add(contribution_extra);
    let mut estimate = correction_estimate(&payload, parent_layout.vision_tokens)?;
    let mut budget = budget_tokens(
        estimate.saturating_add(contribution_extra),
        observed_input_tokens.zip(Some(parent_charged)),
    );
    let trigger_budget = budget;
    let mut new_observation = None;
    let mut layout = parent_layout.clone();
    let mut preparation_error = None;
    let selection = if let Some(caps) = caps.as_ref() {
        let store = Arc::clone(&state.store);
        let registry = Arc::clone(&state.context_registry);
        if registry.ledger().scope_id() != parent_context.source.scope_id
            || registry.ledger().model_generation().map_err(storage)?
                != parent_context.source.model_generation
            || registry.ledger().model_profile().map_err(storage)?
                != Some((
                    parent_context.source.model_generation,
                    parent_context.profile_digest.0.clone(),
                ))
        {
            return Err(ShenkoError::conflict(
                "correction scope/model profile changed; original result retained; no dispatch",
            ));
        }
        let source = StoreHistory::new(
            Arc::clone(&store),
            parent_context.source.scope_id,
            Arc::new(estimate_tokens),
        );
        let history = BoundedHistory {
            history: &source,
            leaf: parent_context.source.leaf_seq,
        };
        let mut window = registry.ledger().current().map_err(storage)?;
        if window.model_generation != parent_context.source.model_generation
            || window.generation < parent_context.source.window_generation
            || window
                .covers_through
                .is_some_and(|through| history.leaf.is_none_or(|leaf| through > leaf))
        {
            return Err(ShenkoError::conflict(
                "correction window is outside its original raw-history boundary; no dispatch",
            ));
        }
        let immutable_tokens = estimate_tokens(
            &parent_payload.system_prompt[..usize::try_from(parent_layout.system_prefix_bytes)
                .map_err(|_| ShenkoError::conflict("frozen system prefix overflow"))?],
        )
        .saturating_add(text_message_tokens(
            &parent_payload.messages[history_end..tail_start],
        ))
        .saturating_add(text_message_tokens(&tail))
        .saturating_add(contribution_extra);
        let mut preparation_caps = caps.clone();
        preparation_caps.max_context = preparation_caps
            .max_context
            .saturating_sub(immutable_tokens);
        if let Some(vision) = &mut preparation_caps.vision {
            vision.max_image_context = vision.max_image_context.saturating_sub(immutable_tokens);
        }
        if preparation_caps.max_context == 0 && budget > caps.max_context {
            return Err(ShenkoError::conflict(format!(
                "correction immutable input budget {immutable_tokens} exhausts available context {}; original result retained; no dispatch",
                caps.max_context
            )));
        }
        // A previous projection may have adopted a window immediately before
        // an R0 CAS refusal/crash. Re-observe it explicitly; never reinterpret
        // the parent's source_commit_seq as evidence for the new window.
        let parent_window = window.id == parent_context.source.window_id;
        if !parent_window {
            let (next_payload, next_layout, observation, unfinished) = project_correction(
                &store,
                &history,
                &window,
                &parent_projection,
                Some(caps),
                &tail,
            )?;
            payload = next_payload;
            layout = next_layout;
            new_observation = Some(observation);
            context.unfinished_call_ids = unfinished;
            estimate = correction_estimate(&payload, layout.vision_tokens)?;
            budget = estimate.saturating_add(contribution_extra);
        }
        let WindowSelection {
            selection,
            preparation_error: error,
        } = select_context_window(
            state,
            &history,
            Some(caps),
            Some(&preparation_caps),
            &mut window,
            budget,
        )?;
        preparation_error = error;
        if new_observation.is_some() || window.id != parent_context.source.window_id {
            // Pressure/adoption may write the ledger. The final projection
            // obtains its own stable execution observation after those writes;
            // Store compares it again inside the R0 transaction.
            let (next_payload, next_layout, observation, unfinished) = project_correction(
                &store,
                &history,
                &window,
                &parent_projection,
                Some(caps),
                &tail,
            )?;
            payload = next_payload;
            layout = next_layout;
            new_observation = Some(observation);
            context.unfinished_call_ids = unfinished;
            estimate = correction_estimate(&payload, layout.vision_tokens)?;
            budget = estimate.saturating_add(contribution_extra);
        }
        selection
    } else {
        ContextSelection::Unavailable {
            reason: "model_context_limit_not_observed".into(),
        }
    };
    if correction_limit(caps.as_ref(), &payload)
        .is_some_and(|limit| budget > limit || estimate > limit)
    {
        if matches!(selection, ContextSelection::Preparing { .. }) {
            return Ok(None);
        }
        if let Some(error) = preparation_error {
            return Err(error);
        }
        return Err(ShenkoError::conflict(format!(
            "corrected input budget {budget} exceeds available context {:?}; original result retained; no dispatch",
            correction_limit(caps.as_ref(), &payload),
        )));
    }
    layout.partitions(&payload)?;
    if let Some(observation) = &new_observation {
        context.source = observation.clone();
    }
    context.observed_input_tokens = observed_input_tokens;
    context.estimated_input_tokens = Some(estimate);
    context.budget_input_tokens = Some(budget);
    context.trigger_budget_input_tokens = Some(trigger_budget);
    context.budget_provenance = format!(
        "complete correction heuristic; {}; contribution max(rendered {contribution_estimate}, declared {declared}); reserved output {}",
        if new_observation.is_some() {
            "new execution projection within the parent's raw scope/leaf; parent seal retained separately"
        } else {
            "immediate parent provider/complete-estimate calibration; original frozen projection retained"
        },
        context.reserved_output_tokens,
    );
    context.selection = selection;
    context.payload_layout = Some(layout);
    context.correction_observation = Some(FrozenCorrectionObservation {
        parent_input_ref: parent_ref.clone(),
        parent_source: parent_context.source.clone(),
        new_execution_projection: new_observation,
        tail_digest: Digest::blake3(&serde_json::to_vec(&serde_json::to_value(&tail)?)?),
        tail_message_count: tail.len() as u64,
    });
    Ok(Some((payload, context)))
}

const OBLIGATION_FRONTIER_ITEMS: usize = 12;
const OBLIGATION_FRONTIER_BYTES: usize = 8 * 1024;

/// Structured references only affect presentation of already-owned facts.
/// They neither execute an action nor grant authority. Native task results
/// have already been authenticated against T2 by stage_input above.
fn obligation_references(events: &[InboxEvent]) -> Vec<EntityId> {
    let mut ids = Vec::new();
    for event in events {
        let id = match (&event.kind, event.source.as_str()) {
            (InboxEventKind::SelfWake, shenko_types::inbox::SUBJECT_WAKE_SOURCE) =>
                event.payload.get("obligation_id").cloned(),
            (InboxEventKind::TaskResult, "native-child") => TaskResult::deserialize(&event.payload).ok()
                .and_then(|result| result.result).and_then(|result| result.get("obligation_id").cloned()),
            _ => None,
        };
        if let Some(id) = id.and_then(|id| serde_json::from_value::<EntityId>(id).ok())
            && !ids.contains(&id) { ids.push(id); }
    }
    ids
}

/// The seal retains each selected original body. The model receives bounded
/// previews with exact identities/digests and can retrieve the whole record;
/// an omitted record is never reported as completed or forgotten.
fn obligation_frontier_text(frontier: &shenko_store::subject_state::ObligationFrontier)
    -> Result<String, ShenkoError> {
    let mut text = format!("\n\n持久义务工作前沿：开放总数 {}，本轮 {}，轮转起点 {}。以下为预览；完整原文仍保留，未展示不表示完成或遗忘。全集：[delegate:history.query:1] {{\"op\":\"obligations\",\"limit\":20}}；按返回page.next作为after续页。全文：[delegate:history.query:1] {{\"op\":\"read_obligation\",\"obligation_id\":\"...\"}}。完成/取消仍需显式终态和理由。\n",
        frontier.total_open, frontier.obligations.len(), frontier.rotation_start);
    let budget = OBLIGATION_FRONTIER_BYTES.saturating_sub(text.len() + frontier.obligations.len())
        / frontier.obligations.len().max(1);
    for obligation in &frontier.obligations {
        let body = serde_json::to_vec(obligation)?;
        let mut description: String = obligation.description.chars().take(128).collect();
        let mut resume: String = obligation.resume_condition.chars().take(64).collect();
        loop {
            let preview = serde_json::to_string(&serde_json::json!({
                "obligation_id": obligation.obligation_id,
                "body_digest": Digest::blake3(&body),
                "description_preview": description,
                "resume_condition_preview": resume,
                "truncated": description != obligation.description || resume != obligation.resume_condition,
                "depends_on_task_id": obligation.depends_on_task_id,
                "due_at": obligation.due_at,
            }))?;
            if preview.len() <= budget {
                text.push_str(&preview);
                text.push('\n');
                break;
            }
            if description.len() >= resume.len() && !description.is_empty() { description.pop(); }
            else if !resume.is_empty() { resume.pop(); }
            else { return Err(ShenkoError::conflict("obligation frontier identity exceeds its projection budget")); }
        }
    }
    Ok(text)
}

pub(super) fn prepare_context(
    state: &mut SubjectLoopState,
    events: &[InboxEvent],
) -> Result<Option<PreparedContextInput>, ShenkoError> {
    // Observe the durable cap before taking this new S0 snapshot. New input is
    // evidence for the model, not an automatic semantic-wait exit.
    state.semantic_wait = state.store.observe_semantic_wait(&state.head)?;
    if let Some(wait) = state.semantic_wait.as_ref().filter(|wait| wait.is_active()) {
        let binding = serde_json::to_value(state.context_binding())?;
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
    // receives a bounded, step-rotated page of cards and decides what, if
    // anything, it means here. A word-matching gate would withhold the unit
    // precisely when the message names a symptom instead of the concept.
    // Unit bodies never travel whole; the model reads the body it actually
    // wants at an exact version through cognition.unit.read.
    let awareness: Vec<String> = state
        .awareness
        .as_ref()
        .map(|catalog| catalog.disclosure_at(state.step.step_no).map(|text| vec![text]))
        .transpose()
        .map_err(|error| {
            ShenkoError::storage(format!("cognition disclosure failed: {error:#}"))
        })?
        .unwrap_or_default();
    let mut frontier = state.store.subject_obligation_frontier(state.agent_id, state.step.step_no,
        &obligation_references(&recovered), OBLIGATION_FRONTIER_ITEMS)?;
    // The once-per-window reminder is READ here and answered by the commit of
    // the request that carries it. Reading it twice is idempotent.
    state.pending_reminder = state.context_registry.pending_rollover_reminder();
    let mut system_prompt = build_system_prompt(state, &awareness, &[]);
    // Leave three quarters of the remaining known input budget to raw input,
    // recovery controls and working history. The normal final budget check
    // still accounts for the whole payload, including this frontier.
    let frontier_tokens = state.context_model_capabilities().as_deref()
        .and_then(|capabilities| u32::try_from(state.max_tokens).ok()
            .and_then(|reserved| input_model_caps(capabilities, reserved)))
        .map(|caps| caps.max_context.saturating_sub(estimate_tokens(&system_prompt)) / 4);
    let mut obligation_text = obligation_frontier_text(&frontier)?;
    while frontier_tokens.is_some_and(|budget| estimate_tokens(&obligation_text) > budget)
        && !frontier.obligations.is_empty() {
        frontier.obligations.pop();
        obligation_text = obligation_frontier_text(&frontier)?;
    }
    if frontier_tokens.is_some_and(|budget| estimate_tokens(&obligation_text) > budget) {
        obligation_text = format!("\n义务开放 {}；本轮预算只容纳查询指针，完整记录仍可用 history.query 的 obligations/read_obligation 读取。\n", frontier.total_open);
    }
    let obligations = frontier.obligations;
    system_prompt.push_str(&obligation_text);
    let registry = Arc::clone(&state.context_registry);
    let binding = state.context_binding();
    let profile_digest = bind_context_profile(state)?;
    let model_generation = registry.ledger().model_generation().map_err(storage)?;
    let scope_id = EntityId(state.head.branch_id.0);
    let source = StoreHistory::with_cache(Arc::clone(&state.store), scope_id, Arc::new(estimate_tokens),
        Arc::clone(&state.context_history_cache));
    let history = BoundedHistory { leaf: source.leaf_seq().map_err(storage)?, history: &source };
    let reserved_output_tokens = u32::try_from(state.max_tokens)
        .map_err(|_| ShenkoError::conflict("output reservation exceeds token budget type"))?;
    let caps = state.context_model_capabilities().as_deref()
        .and_then(|caps| input_model_caps(caps, reserved_output_tokens));
    if caps.as_ref().is_some_and(|caps| caps.max_context == 0) {
        return Err(ShenkoError::conflict("output reservation leaves no input context"));
    }
    let mut window = registry.ledger().current().map_err(storage)?;
    let observed_input_tokens = state.previous_input_tokens.filter(|_|
        binding.is_some() && state.previous_input_binding.as_ref() == binding);
    let previous_anchor = observed_input_tokens.zip(state.previous_estimated_input_tokens);
    // The idle observation is content too: the installed unit
    // that provides `cap.idle-perception` words what "no new input" means to
    // this subject. The compiled sentence stays as the fallback.
    let idle_text = state.idle_perception();
    let StableProjection { mut payload, mut estimated_input_tokens, mut unfinished_call_ids,
        mut source_commit_seq, mut payload_layout } = project_payload_stable(
        &state.store, scope_id, &window, &history, system_prompt.clone(), &input_raw_event_ids,
        PayloadProjectionOptions { caps: caps.as_ref(), max_tokens: state.max_tokens, idle_text: Some(&idle_text) },
    )?;
    let current_estimate = estimated_input_tokens
        .ok_or_else(|| ShenkoError::conflict("current projection has no trustworthy token estimate"))?;
    let trigger_budget_input_tokens = budget_tokens(current_estimate, previous_anchor);
    let WindowSelection { selection, preparation_error } = select_context_window(
        state, &history, caps.as_ref(), caps.as_ref(), &mut window,
        trigger_budget_input_tokens,
    )?;
    if matches!(selection, ContextSelection::Compacted { .. }) {
        StableProjection { payload, estimated_input_tokens, unfinished_call_ids, source_commit_seq, payload_layout } = project_payload_stable(
            &state.store, scope_id, &window, &history, system_prompt, &input_raw_event_ids,
            PayloadProjectionOptions { caps: caps.as_ref(), max_tokens: state.max_tokens, idle_text: Some(&idle_text) },
        )?;
    }
    let selected_estimate = estimated_input_tokens
        .ok_or_else(|| ShenkoError::conflict("selected projection has no trustworthy token estimate"))?;
    // The prepared contributions are collected BEFORE the final budget
    // check, so their cost is charged together with the context and the
    // tool cost and can never be added after the check.
    let collected = collect_contributions(
        &state.store, state.contribution_registry.as_deref(),
        &shenko_context_policy::contribution::scope_wide(scope_id),
    )?;
    let mut contribution_estimate = 0;
    let (contributions, contribution_omissions, contribution_tokens) =
        match collected {
        Some(collected) => {
            // The model-facing material of a contribution is the text of the
            // sealed document its reference points at. Its provenance
            // (producer, version, artifact digest) is NOT put in front of
            // the model: that reaches the raw call record through the seal.
            if let Some(text) = collected.material {
                contribution_estimate = estimate_tokens(&text);
                payload.messages.push(InferenceMessage::text("user", text));
                payload_layout.contribution_message_count += 1;
            }
            (
                collected.plan.contributions.iter().map(serde_json::to_value)
                    .collect::<Result<Vec<_>, _>>()?,
                collected.plan.omitted.iter().map(serde_json::to_value)
                    .collect::<Result<Vec<_>, _>>()?,
                collected.plan.total_tokens,
            )
        }
        None => (Vec::new(), Vec::new(), 0),
    };
    // Only this new text part changed. Preserve the selected window's image
    // estimate and count its body/envelope once, without rescanning history.
    let final_estimate = selected_estimate.saturating_add(contribution_estimate);
    estimated_input_tokens = Some(final_estimate);
    let charged_estimate = selected_estimate.saturating_add(contribution_estimate.max(
        u32::try_from(contribution_tokens).unwrap_or(u32::MAX),
    ));
    // Calibrate the complete payload, not a prefix whose previous-use floor
    // may already include contribution material. A new compacted window
    // deliberately starts from its own estimate, as before.
    let (selected_budget_input_tokens, budget_provenance) = match (&selection, previous_anchor) {
        (ContextSelection::Compacted { .. }, _) =>
            (charged_estimate, "selected compacted heuristic".into()),
        (_, Some((actual, previous))) => (
            budget_tokens(charged_estimate, previous_anchor),
            format!("max(final charged estimate, previous provider input {actual} + positive delta from previous heuristic {previous})"),
        ),
        (_, None) => (charged_estimate, "final charged estimate; no matching previous provider/estimate anchor".into()),
    };
    let has_image = payload.messages.iter().flat_map(|message| &message.parts)
        .any(InferencePart::is_image);
    let input_limit = caps.as_ref().and_then(|caps| if has_image {
        caps.image_request_context()
    } else {
        Some(caps.max_context)
    });
    if input_limit.is_some_and(|limit| selected_budget_input_tokens > limit || final_estimate > limit) {
        if matches!(selection, ContextSelection::Preparing { .. }) {
            return Ok(None);
        }
        if let Some(error) = preparation_error {
            return Err(error);
        }
        return Err(ShenkoError::conflict(format!(
            "projected input budget {selected_budget_input_tokens} exceeds available context {input_limit:?}; no dispatch"
        )));
    }
    Ok(Some(PreparedContextInput {
        obligations,
        events: recovered, semantic_wait: state.semantic_wait.clone(),
        payload,
        contributions,
        contribution_omissions,
        contribution_tokens,
        context: FrozenContextInput {
            source: SubjectContextRef { scope_id, source_commit_seq,
                leaf_seq: history.leaf, input_raw_event_ids,
                window_id: window.id, window_generation: window.generation, model_generation },
            profile_digest, capabilities: state.context_model_capabilities().clone(),
            observed_input_tokens,
            estimated_input_tokens,
            budget_input_tokens: Some(selected_budget_input_tokens),
            trigger_budget_input_tokens: Some(trigger_budget_input_tokens),
            budget_provenance: format!("{budget_provenance}; contribution max(rendered {contribution_estimate}, declared {contribution_tokens}); reserved output {reserved_output_tokens}"),
            reserved_output_tokens,
            unfinished_call_ids,
            token_counter: "heuristic:cjk-1-other-4@1; not a provider tokenizer".into(),
            policy_contract: CONTRACT_VERSION, selection,
            payload_layout: Some(payload_layout), correction_observation: None,
        },
    }))
}
