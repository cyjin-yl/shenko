//! Continuous subject loop — SPEC §5.1, §41.5
//! Real S0/S1 step state machine. Never terminates on empty inbox.
//! Semantic wait hard-max 600s. Yields inference slot after each step.
use crate::inbox::{Inbox, InboxEvent};
use crate::context_projection::{FrozenContextInput, estimate_payload_tokens, prepare_context};
use crate::outbox::OutboxMessage;
use crate::quota_gate::{QuotaGate, quota_dispatch_gate, observe_quota_response, settle_quota_terminal};
use shenko_identity::IdentityService;
use shenko_kernel::hooks::{HookPayload, HookRegistry};
use shenko_types::commit::CommitRef;
use shenko_types::error::ShenkoError;
use shenko_types::identity::{
    IdentityIntent, IdentityOutcome, IdentityProfile, IdentityReceipt, IdentitySource,
};
use shenko_types::ids::{EntityId, RecordRevision};
use shenko_types::object::ObjectRef;
use shenko_types::output::OutputAction;
use shenko_types::subject::{
    DelegationIntent, Disposition, EventDisposition, InferenceRetryFact, MemoryIntent,
    MemoryIntentKind, PolicyIntent, PolicyKind, SemanticWait, SemanticWaitReceipt,
    SemanticWaitOutcome, StepDecision, SubjectBranchHead, SubjectRetryEvidence, SubjectStep,
    SubjectMode, SubjectStepState,
};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Recoverable S0 input, including the immutable request projection and original
/// event payloads. A list of UUIDs alone cannot reconstruct a result's dispositions.
#[derive(serde::Serialize, serde::Deserialize)]
struct FrozenStepInput {
    schema_version: u32,
    step_id: EntityId,
    step_no: u64,
    events: Vec<InboxEvent>,
    provider_binding: Option<shenko_broker::request::ProviderBinding>,
    payload: shenko_broker::request::InferencePayload,
    context: FrozenContextInput,
    semantic_wait: Option<SemanticWait>,
    identity: Option<IdentityProfile>,
    identity_provider_revision: String,
}

fn read_sealed(
    store: &shenko_store::store::Store,
    object: &ObjectRef,
) -> Result<Vec<u8>, ShenkoError> {
    let bytes = store.cas.read_object(&object.digest)?;
    if object.locator.is_empty()
        || bytes.len() as u64 != object.bytes
        || shenko_types::object::Digest::blake3(&bytes) != object.digest
    {
        return Err(ShenkoError::ObjectClosureIncomplete { missing_count: 1 });
    }
    Ok(bytes)
}

fn frozen_input(
    store: &shenko_store::store::Store,
    step: &SubjectStep,
) -> Result<FrozenStepInput, ShenkoError> {
    let object = step
        .input_plan_ref
        .as_ref()
        .ok_or_else(|| ShenkoError::conflict("recovery_blocked: missing frozen input"))?;
    let input: FrozenStepInput = serde_json::from_slice(&read_sealed(store, object)?)
        .map_err(|_| ShenkoError::conflict("recovery_blocked: obsolete/incomplete S0 protocol: explicit reconciliation/migration required"))?;
    if input.schema_version != 11 || input.step_id != step.step_id || input.step_no != step.step_no {
        return Err(ShenkoError::conflict(
            "recovery_blocked: frozen input identity mismatch",
        ));
    }
    Ok(input)
}

fn validate_resume(
    store: &shenko_store::store::Store,
    step: &SubjectStep,
) -> Result<Option<FrozenStepInput>, ShenkoError> {
    use SubjectStepState::*;
    let no_request = step.active_request_id.is_none() && step.active_attempt_id.is_none();
    match step.state {
        Planned if no_request => return Ok(None),
        Preparing if no_request => {}
        RetryPending => {
            let object = step
                .retry_reason_ref
                .as_ref()
                .ok_or_else(|| ShenkoError::conflict("recovery_blocked: missing retry evidence"))?;
            let evidence: SubjectRetryEvidence =
                serde_json::from_slice(&read_sealed(store, object)?)?;
            if step.active_attempt_id.is_some() || !evidence.permits_retry(step.active_request_id) {
                return Err(ShenkoError::conflict(
                    "recovery_blocked: retry outcome not conclusive",
                ));
            }
        }
        ResultRecorded if step.active_request_id.is_some() => {
            if step.model_result_ref.is_none() || step.model_outcome_ref.is_none() {
                return Err(ShenkoError::conflict(
                    "recovery_blocked: missing terminal model evidence; explicit migration required",
                ));
            }
        }
        _ => {
            return Err(ShenkoError::conflict(format!(
                "recovery_blocked: step {} is {:?}; reconcile original request/attempt before inference",
                step.step_id, step.state,
            )));
        }
    }
    frozen_input(store, step).map(Some)
}

pub struct SubjectLoopState {
    pub agent_id: EntityId,
    pub head: SubjectBranchHead,
    pub step: SubjectStep,
    pub inbox: Arc<Inbox>,
    pub semantic_wait: Option<SemanticWait>,
    pub semantic_wait_receipt: Option<SemanticWaitReceipt>,
    pub store: Arc<shenko_store::store::Store>,
    pub broker: Option<shenko_broker::broker::BrokerHandle>,
    pub hooks: Option<Arc<HookRegistry>>,
    pub max_tokens: u64,
    /// Only provider-reported input usage of the previous committed result.
    pub(super) previous_input_tokens: Option<u32>,
    /// Heuristic projection count, never confused with provider usage.
    pub(super) previous_estimated_input_tokens: Option<u32>,
    pub(super) previous_input_binding: Option<shenko_broker::request::ProviderBinding>,
    pub(super) model_capabilities: Option<Arc<shenko_broker::request::ProviderCapabilities>>,
    pub(super) context_registry: Arc<shenko_context_policy::PolicyRegistry>,
    native_capability_sections: Vec<String>,
    /// Rolling goals the subject is pursuing.
    pub goals: Vec<String>,
    /// Outbound path for completed model output — SPEC §5.1, §21.12.
    pub outbox_tx: Option<tokio::sync::broadcast::Sender<OutboxMessage>>,
    /// Identity revision fixed for the current step — SPEC §6. Reloaded only
    /// at the S0 boundary, so a change applies from the next step on.
    pub identity: Option<IdentityProfile>,
    pub identity_receipt: Option<IdentityReceipt>,
    pub(super) plugin_context: cordis_core::Context,
    identity_service: Arc<IdentityService>,
    /// Installable awareness units available to this subject. Their bodies are
    /// advisory material for the model, never a dispatch table.
    pub(super) awareness: Option<Arc<shenko_awareness_core::AwarenessRegistry>>,
}

impl SubjectLoopState {
    /// 从真实 head/唯一后继恢复；未知模型状态不允许重新发起推理。
    pub fn new(
        agent_id: EntityId,
        inbox: Arc<Inbox>,
        store: Arc<shenko_store::store::Store>,
        plugin_context: cordis_core::Context,
    ) -> Result<Self, ShenkoError> {
        let (head, step) = store.load_subject(agent_id)?;
        if !matches!(head.mode, SubjectMode::Active | SubjectMode::Paused) {
            return Err(ShenkoError::conflict("subject controller is not runnable; explicit recovery required"));
        }
        if head.mode == SubjectMode::Paused && step.state != SubjectStepState::Planned {
            return Err(ShenkoError::conflict("paused subject has no planned boundary"));
        }
        let resumed_input = validate_resume(&store, &step)?;
        let identity_service = plugin_context
            .try_service::<IdentityService>()
            .map_err(|e| ShenkoError::other(format!("IDENTITY_PROVIDER_UNAVAILABLE: {e}")))?;
        let context_registry = plugin_context.try_service::<shenko_context_policy::PolicyRegistry>()
            .map_err(|e| ShenkoError::other(format!("CONTEXT_PROVIDER_UNAVAILABLE: {e}")))?;
        if resumed_input
            .as_ref()
            .is_some_and(|input| input.identity_provider_revision != identity_service.revision())
        {
            return Err(ShenkoError::conflict(
                "IDENTITY_PROVIDER_REVISION_MISMATCH: migrate/reconcile the sealed request before resuming",
            ));
        }
        let (previous_input_tokens, previous_input_binding, previous_estimated_input_tokens) = if let Some(id) = head.committed_step_id {
            let previous = store
                .get_step(&head.branch_id, id)?
                .ok_or_else(|| ShenkoError::not_found("committed subject step"))?;
            if previous.state != SubjectStepState::Committed
                || previous.commit_ref != Some(head.committed_at)
            {
                return Err(ShenkoError::conflict("committed head/step mismatch"));
            }
            store.require_subject_raw_coverage(&head, &previous)?;
            let (binding, estimated) = sealed_previous_projection(&store, &previous)?;
            let observed = observed_input_tokens(&store.recorded_model_outcome(&previous)?, binding.as_ref());
            (observed, binding, estimated)
        } else {
            (None, None, None)
        };
        let identity = if step.state == SubjectStepState::Planned {
            store.latest_identity(agent_id)?
        } else {
            resumed_input.and_then(|input| input.identity)
        };
        let identity_receipt = head
            .committed_step_id
            .map(|id| store.identity_receipt(id))
            .transpose()?
            .flatten();
        let semantic_wait = store.semantic_wait(agent_id)?;
        let semantic_wait_receipt = head
            .committed_step_id
            .map(|id| store.semantic_wait_receipt(id))
            .transpose()?
            .flatten();
        Ok(Self {
            agent_id,
            head,
            step,
            inbox,
            semantic_wait,
            semantic_wait_receipt,
            store,
            broker: None,
            hooks: None,
            max_tokens: 4096,
            previous_input_tokens,
            previous_input_binding,
            previous_estimated_input_tokens,
            model_capabilities: None,
            context_registry,
            native_capability_sections: vec![],
            goals: vec![],
            outbox_tx: None,
            identity,
            identity_receipt,
            plugin_context,
            identity_service,
            awareness: None,
        })
    }

    pub fn with_broker(mut self, broker: shenko_broker::broker::BrokerHandle) -> Self {
        self.broker = Some(broker);
        self
    }

    pub fn with_model_capabilities(mut self, capabilities: Option<shenko_broker::request::ProviderCapabilities>) -> Self {
        self.model_capabilities = capabilities.map(Arc::new);
        self
    }

    pub fn with_hooks(mut self, registry: Arc<HookRegistry>) -> Self {
        self.hooks = Some(registry);
        self
    }

    /// Installed native definitions the child registry actually exposes. They
    /// belong to the tool contract, so they travel with the sealed request.
    pub fn with_native_capabilities(mut self, sections: Vec<String>) -> Self {
        self.native_capability_sections = sections;
        self
    }

    /// Attach the installed awareness units. Every enabled unit travels as
    /// advisory material for the step; the model decides what it means here, so
    /// no kernel branch is created per topic.
    pub fn with_awareness(
        mut self,
        registry: Arc<shenko_awareness_core::AwarenessRegistry>,
    ) -> Self {
        self.awareness = Some(registry);
        self
    }

    /// Attach the outbound timeline broadcaster — SPEC §5.1, §21.12.
    pub fn with_outbox(mut self, tx: tokio::sync::broadcast::Sender<OutboxMessage>) -> Self {
        self.outbox_tx = Some(tx);
        self
    }
}

/// Run the continuous subject loop. SPEC §5.1, §41.5.
/// - Never stops for empty inbox
/// - S0: seal SubjectStep as Planned → Preparing
/// - AwaitingModel: real inference via broker
/// - S1: commit as Committed, record next_step_id
/// - Semantic wait: max 600s, same model, same thought stream
pub async fn run_subject_loop(
    mut state: SubjectLoopState,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<(), ShenkoError> {
    info!(agent_id=%state.agent_id, "subject loop started");

    let result = loop {
        if cancel.is_cancelled() {
            info!("subject loop: shutdown");
            break Ok(());
        }

        if state.head.mode == SubjectMode::Paused {
            tokio::select! {
                _ = cancel.cancelled() => break Ok(()),
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            }
            let (head, step) = match state.store.load_subject(state.agent_id) {
                Ok(subject) => subject,
                Err(error) => break Err(error),
            };
            if !matches!(head.mode, SubjectMode::Active | SubjectMode::Paused)
                || head.controller_epoch != state.head.controller_epoch
                || head.software_generation != state.head.software_generation
                || head.next_step_id != state.head.next_step_id
                || step.state != SubjectStepState::Planned
            {
                break Err(ShenkoError::conflict("paused subject changed; reopen and reconcile"));
            }
            state.head = head;
            state.step = step;
            continue;
        }

        if state.step.state == SubjectStepState::Planned {
            match state.store.pause_subject_at_boundary(&state.head) {
                Ok(Some(head)) => {
                    state.head = head;
                    info!(agent_id=%state.agent_id, "subject paused at a durable step boundary");
                    continue;
                }
                Ok(None) => {}
                Err(error) => break Err(error),
            }
        }

        let seq = state.step.step_no;

        // Collect available events — short timeout to stay responsive
        let mut events = if state.step.state == SubjectStepState::Planned {
            state.inbox.recv_batch(32, 80).await
        } else {
            // Never consume new input into a recovered, already frozen step.
            vec![]
        };
        // An ingress replay may already be queued when ResultRecorded resumes S1.
        // Recheck L at consumption time so that stable IDs are never executed twice.
        let filtered = events
            .into_iter()
            .map(|event| {
                state
                    .store
                    .event_disposition_commit(state.agent_id, event.event_id)
                    .map(|commit| if commit.is_none() { Some(event) } else { None })
            })
            .collect::<Result<Vec<_>, _>>();
        events = match filtered {
            Ok(events) => events.into_iter().flatten().collect(),
            Err(error) => break Err(error),
        };
        // Execute one subject step (S0 → AwaitingModel → Committed)
        if let Err(error) = execute_step(seq, &events, &mut state, &cancel).await
        {
            // 失败或结果未知后关闭本运行器；不能在旧内存状态中造一个新步骤。
            warn!(seq, err=%error, "主体恢复阻塞，停止新决定，等待重开和对账");
            break Err(error);
        }

        // Yield inference slot to children — cooperative multitasking. SPEC §5.1
        tokio::task::yield_now().await;
    };

    // Keeping Arc<Inbox> in main must not leave a dead consumer accepting input.
    state.inbox.close().await;
    info!("subject loop terminated");
    result
}

/// Role and output contract of the subject. It names the framework, never the
/// instance, and does not vary with identity revisions (R054). SPEC §5.1, §6.
pub(crate) const SUBJECT_CONTRACT: &str = "你是一个持续运行的个人 agent，运行在 Shenko 框架上。\n\
     你有持续的内心生活：关注、兴趣、承诺、疑问。\n\
     你在没有外部消息时也继续思考——空闲不是停止的理由。\n\
     思考是连续的，上一步的想法会成为这一步的起点。\n\
     普通正文是自己的想法，可显示在系统时间线，但不会作为 QQ 消息发送。要让 QQ 对方收到的话，必须通过回复控制行发送。";

/// System prompt for the continuous subject: contract → persona → per-step
/// suffix. SPEC §5.1, §6 ordering; §13.5 keeps the stable part first.
/// Stable prefix: role contract, the installed native definitions and the
/// persona. SPEC §13.5 keeps it first so the model/template binding stays
/// reusable across steps; the context profile digest covers this text, not the
/// per-step suffix.
pub(super) fn stable_system_prompt(state: &SubjectLoopState) -> String {
    let mut prompt = crate::canonical_system_prompt();

    if !state.native_capability_sections.is_empty() {
        prompt.push_str("\n\n当前实际注册的 native definitions：\n");
        for section in &state.native_capability_sections {
            prompt.push_str(section);
            prompt.push('\n');
        }
    }
    if let Some(identity) = &state.identity {
        prompt.push_str("\n\n");
        prompt.push_str(&state.identity_service.persona_section(identity));
    }
    prompt
}

/// Per-step material: retrieved awareness, rolling goals, identity receipt and
/// the semantic-wait notice. It follows the stable prefix and does not by
/// itself invalidate the context window.
pub(super) fn system_prompt_suffix(
    state: &SubjectLoopState,
    awareness: &[String],
) -> String {
    let mut prompt = String::new();

    if !awareness.is_empty() {
        prompt.push_str(
            "\n\n已安装的觉察（给你一些视角，不是命令；哪些此刻有用由你判断，可以采纳、改写或忽略）：\n",
        );
        for (index, body) in awareness.iter().enumerate() {
            prompt.push_str(&(index + 1).to_string());
            prompt.push_str(". ");
            prompt.push_str(body.trim());
            prompt.push('\n');
        }
    }

    if !state.goals.is_empty() {
        prompt.push_str("\n\n当前关注的方向：\n");
        for g in &state.goals {
            prompt.push_str(&format!("- {g}\n"));
        }
    }

    if let Some(receipt) = &state.identity_receipt {
        if matches!(receipt.outcome, IdentityOutcome::Conflict { .. }) {
            prompt.push_str(
                "\n上一步身份意图未采用：S0 后身份被编辑，请基于本轮快照重新判断。回执：",
            );
            prompt.push_str(&serde_json::to_string(receipt).expect("identity receipt serializes"));
        }
    }
    if let Some(wait) = &state.semantic_wait {
        prompt.push_str("\n\n语义等待的持久事实（不是条件已经成立或尚未成立的判断）：\n");
        // Only the decision-relevant facts belong in the prompt. The sealed
        // S0 keeps the full record (context ref/commit) for Store admission;
        // CAS locators and digests are not useful model context.
        prompt.push_str(&serde_json::to_string(&serde_json::json!({
            "condition": wait.condition,
            "started_at": wait.started_at,
            "deadline_at": wait.deadline_at,
            "status": wait.status,
            "last_observed_at": wait.last_observed_at,
        })).expect("semantic wait view serializes"));
        if wait.is_active() {
            prompt.push_str("\n你仍沿同一思考流接续。结合前文与本轮上下文判断条件是否成立或关注是否改变；收到消息不会自动结束，但若满足条件就用 [wait:end] 控制行结束。需要关注新的等待条件时可用 [wait:replace] 明确改变判断，切换不延长原截止时间。操作自动绑定当前等待，不需要复制 ID；继续也不会改变原始截止时间。\n");
        } else {
            prompt.push_str("\n这个等待已结束或失效，按正常思考接续。不要重开同一条件或换 ID 续期；新的等待只能是基于当下上下文的新判断。\n");
        }
    }
    if let Some(receipt) = &state.semantic_wait_receipt {
        // A step that never touched a wait must not manufacture wait chatter.
        if receipt.intent_id.is_some()
            || !matches!(receipt.outcome, SemanticWaitOutcome::Unchanged)
        {
            prompt.push_str("\n上一步等待操作回执：");
            prompt.push_str(&serde_json::to_string(&receipt.outcome)
                .expect("semantic wait receipt serializes"));
            prompt.push('\n');
        }
    }

    prompt
}

pub(super) fn build_system_prompt(
    state: &SubjectLoopState,
    awareness: &[String],
) -> String {
    let mut prompt = stable_system_prompt(state);
    prompt.push_str(&system_prompt_suffix(state, awareness));
    prompt
}

fn observed_input_tokens(
    outcome: &shenko_types::subject::SubjectModelOutcome,
    binding: Option<&shenko_broker::request::ProviderBinding>,
) -> Option<u32> {
    use shenko_broker::request::ProviderBinding;

    #[derive(serde::Deserialize)]
    struct Response<T> { usage: Option<T> }
    #[derive(serde::Deserialize)]
    struct LocalUsage { prompt_tokens: Option<u64> }
    #[derive(serde::Deserialize)]
    struct AnthropicUsage { input_tokens: Option<u64> }

    let normalized = outcome.usage.as_ref()?.get("input_tokens")?.as_u64()?;
    // Older normalized outcomes may contain a fabricated zero. Only the raw
    // field selected by the sealed S0 provider binding can substantiate it.
    let raw = match binding? {
        ProviderBinding::Local { .. } => serde_json::from_str::<Response<LocalUsage>>(&outcome.raw_response)
            .ok()?.usage?.prompt_tokens?,
        ProviderBinding::Anthropic { .. } => serde_json::from_str::<Response<AnthropicUsage>>(&outcome.raw_response)
            .ok()?.usage?.input_tokens?,
        ProviderBinding::Stub { .. } => return None,
    };
    if raw != normalized { return None; }
    u32::try_from(raw).ok()
}

fn sealed_previous_projection(
    store: &shenko_store::store::Store,
    step: &SubjectStep,
) -> Result<(Option<shenko_broker::request::ProviderBinding>, Option<u32>), ShenkoError> {
    let Some(object) = &step.input_plan_ref else { return Ok((None, None)) };
    let input: serde_json::Value = serde_json::from_slice(&read_sealed(store, object)?)?;
    if input.get("schema_version").and_then(serde_json::Value::as_u64) != Some(11) {
        // Older committed history can still be read, but its prior usage is
        // not silently bound to this new projection.
        return Ok((None, None));
    }
    let binding = input.get("provider_binding").cloned()
        .ok_or_else(|| ShenkoError::conflict("sealed provider binding missing"))
        .and_then(|value| Ok(serde_json::from_value(value)?))?;
    let estimated = match input.pointer("/context/estimated_input_tokens").and_then(serde_json::Value::as_u64) {
        Some(value) => Some(u32::try_from(value).map_err(|_| ShenkoError::conflict("sealed estimate overflow"))?),
        None => {
            let payload = input.get("payload").cloned()
                .ok_or_else(|| ShenkoError::conflict("sealed request payload missing"))?;
            estimate_payload_tokens(&serde_json::from_value(payload)?, 0)
        }
    };
    Ok((binding, estimated))
}


/// CAS 失败直接返回，绝不制造空 locator 后继续提交。§41.2.2
fn seal_step_object(
    store: &shenko_store::store::Store,
    _seq: u64,
    data: &[u8],
    media_type: &str,
    namespace: &str,
    codec_name: &str,
    codec_major: u32,
) -> Result<ObjectRef, ShenkoError> {
    Ok(store
        .cas
        .seal_object(data, media_type, namespace, codec_name, codec_major)?
        .object)
}

/// Execute one subject step: S0 (Planned→Preparing) → AwaitingModel → S1 (Committed).
/// Returns the commit ref if S1 succeeded.
async fn execute_step(
    seq: u64,
    events: &[InboxEvent],
    state: &mut SubjectLoopState,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Option<CommitRef>, ShenkoError> {
    let store = Arc::clone(&state.store);
    // Pin one graph through all phases of this invocation. A hot swap must not
    // combine an old BeforeInference with a new AfterInference handler set.
    let hooks_ref = state.hooks.as_ref().map(|registry| registry.snapshot());

    let step_id = state.step.step_id;
    let next_step_id = EntityId::new();
    let mut step = state.step.clone();
    if step.step_no != seq {
        return Err(ShenkoError::conflict(
            "step is not the registered successor",
        ));
    }
    let resumed_input = validate_resume(&store, &step)?;
    let input = if step.state == SubjectStepState::Planned {
        state.identity_service = state
            .plugin_context
            .try_service::<IdentityService>()
            .map_err(|e| ShenkoError::other(format!("IDENTITY_PROVIDER_UNAVAILABLE: {e}")))?;
        state.identity = store.latest_identity(state.agent_id)?;
        let prepared = match prepare_context(state, events) {
            Ok(prepared) => prepared,
            Err(ShenkoError::ResnapshotRequired { reason }) => {
                // No S0 seal or model admission exists. The outer loop yields
                // and rebuilds this same Planned step from durable staging.
                tracing::debug!(seq, %reason, "S0 source advanced; reprojecting planned step");
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let input = FrozenStepInput {
            schema_version: 11,
            step_id,
            step_no: seq,
            events: prepared.events,
            provider_binding: state.broker.as_ref().map(|b| b.binding().clone()),
            payload: prepared.payload,
            context: prepared.context,
            semantic_wait: prepared.semantic_wait,
            identity: state.identity.clone(),
            identity_provider_revision: state.identity_service.revision().into(),
        };
        step.input_plan_ref = Some(seal_step_object(
            &store,
            seq,
            &serde_json::to_vec(&input)?,
            "application/json",
            "input_plans",
            "subject_step_input",
            11,
        )?);
        step.context_revision = Some(seq);
        step.state = SubjectStepState::Preparing;
        let expected = step.record_revision;
        step.record_revision = RecordRevision(expected.0 + 1);
        match store.upsert_step(&step, &state.head, expected) {
            Ok(_) => {}
            Err(ShenkoError::ResnapshotRequired { reason }) => {
                tracing::debug!(seq, %reason, "S0 CAS rejected stale source; reprojecting planned step");
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
        state.step = step.clone();
        input
    } else {
        resumed_input.ok_or_else(|| ShenkoError::conflict("missing validated S0"))?
    };
    let events = input.events.as_slice();
    let in_semantic_wait = input.semantic_wait.as_ref().is_some_and(SemanticWait::is_active);
    let input_identity = input.identity.clone();
    let (model_output, stop_reason) = if step.state == SubjectStepState::ResultRecorded {
        // The actual sealed result is authoritative; no broker admission on this path.
        let object = step
            .model_result_ref
            .as_ref()
            .ok_or_else(|| ShenkoError::conflict("missing recorded result"))?;
        let outcome = store.recorded_model_outcome(&step)?;
        (
            Some(
                String::from_utf8(read_sealed(&store, object)?)
                    .map_err(|e| ShenkoError::storage(e.to_string()))?,
            ),
            outcome.stop_reason,
        )
    } else {
        if step.state == SubjectStepState::Preparing {
            if let Some(reg) = &hooks_ref {
                let dispatch = reg
                    .run_phase(&HookPayload::BeforeInference {
                        request_id: step_id,
                        agent_id: state.agent_id,
                        model: input
                            .provider_binding
                            .as_ref()
                            .map(|binding| binding.model())
                            .unwrap_or("unbound")
                            .into(),
                    })
                    .await;
                debug!(seq, hooks=%dispatch.results.len(), graph=%dispatch.graph_digest, "PreModel hooks ran");
                if dispatch.stopped_early {
                    // A stop_unit contract cut the dispatch: the unit is not
                    // allowed, so nothing is submitted to the broker. The
                    // durable Preparing step stays resumable; reconciliation
                    // of the failing handler decides whether it retries.
                    return Err(ShenkoError::conflict(
                        "pre-inference hook dispatch stopped the unit; frozen preparation remains resumable",
                    ));
                }
            }
        }
        // Close is not abort. Before dispatch, the frozen Preparing/RetryPending
        // fact is sufficient to resume. After dispatch, keep waiting for real evidence.
        if cancel.is_cancelled() {
            return Ok(None);
        }
        let broker = state
            .broker
            .as_ref()
            .ok_or_else(|| ShenkoError::other("no broker; frozen preparation remains resumable"))?;
        if input.provider_binding.as_ref() != Some(broker.binding()) {
            return Err(ShenkoError::Protocol(shenko_types::error::ProtocolError::new(
                "MODEL_PROVIDER_BINDING_MISMATCH", shenko_types::error::ErrorScope::Request,
                shenko_types::error::RetryStrategy::NewRevision,
            ).with_details(serde_json::json!({"reason":"explicit S0 migration/reconciliation required; request not dispatched"}))));
        }
        // The sealed provider request id is immutable: every wire send gets
        // its own durable admission identity (send) instead. A conclusively
        // rejected send retries under a fresh send id with its own budget
        // and pacing; the prior send keeps its armed unknown charge.
        let request_id = step.active_request_id.unwrap_or_else(EntityId::new);
        let claimed_send = match quota_dispatch_gate(
            &store,
            broker.binding(),
            request_id,
            cancel,
            None,
        )
        .await
        {
            Ok(QuotaGate::Ready(send_id)) => send_id,
            Ok(QuotaGate::Cancelled) => return Ok(None),
            Err(error) => return Err(error),
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let req = shenko_broker::request::InferenceRequest {
            request_id,
            agent_id: state.agent_id,
            conversation_id: state.agent_id,
            provider_binding: broker.binding().clone(),
            priority: shenko_broker::request::InferencePriority::Subject,
            epoch: state.head.controller_epoch,
            payload: input.payload.clone(),
            result_tx: tx,
        };
        step.state = SubjectStepState::AwaitingModel;
        step.active_request_id = Some(request_id);
        let expected = step.record_revision;
        step.record_revision = RecordRevision(expected.0 + 1);
        match claimed_send {
            // Under quota the CAS itself claims the exact send the gate
            // armed, in the same transaction — never a mutable "latest".
            Some(send_id) => store.upsert_step_claiming_send(
                &step,
                &state.head,
                expected,
                send_id,
            )?,
            None => store.upsert_step(&step, &state.head, expected)?,
        };
        state.step = step.clone();
        // A failed channel send proves this admission was never enqueued. A crash
        // between the write and send cannot prove that, and stays AwaitingModel.
        if broker.submit(req).is_err() {
            record_retry(state, InferenceRetryFact::NeverDispatched)?;
            return Err(ShenkoError::other(
                "broker admission rejected; never_dispatched persisted",
            ));
        }
        let result = rx
            .await
            .map_err(|_| ShenkoError::other("model outcome unknown; reconcile original request"))?;
        if result.request_id != request_id {
            return Err(ShenkoError::conflict(
                "model result request identity mismatch",
            ));
        }
        // Response evidence binds to the exact send this step dispatched —
        // durable claim lineage, never a reversed "latest" guess.
        if let Some(send_id) = claimed_send {
            observe_quota_response(&store, broker.binding(), &result, request_id, send_id);
        }
        use shenko_broker::request::InferenceStatus;
        match result.status {
            InferenceStatus::QueuedCapacityWait => {
                record_retry(state, InferenceRetryFact::NeverDispatched)?;
                return Err(ShenkoError::other(
                    "broker capacity rejection; never_dispatched persisted",
                ));
            }
            InferenceStatus::Rejected { http_status: 429 } => {
                record_retry(
                    state,
                    InferenceRetryFact::ProviderRejected { http_status: 429 },
                )?;
                return Err(ShenkoError::other(
                    "provider rejection persisted; same frozen request may resume",
                ));
            }
            InferenceStatus::Rejected { http_status } => {
                return Err(ShenkoError::Protocol(
                    shenko_types::error::ProtocolError::new(
                        "PROVIDER_REJECTED",
                        shenko_types::error::ErrorScope::Request,
                        shenko_types::error::RetryStrategy::NewRevision,
                    )
                    .with_details(serde_json::json!({"http_status":http_status})),
                ));
            }
            InferenceStatus::InvalidRequest { code } => {
                return Err(ShenkoError::Protocol(
                    shenko_types::error::ProtocolError::new(
                        code.as_str(),
                        shenko_types::error::ErrorScope::Request,
                        shenko_types::error::RetryStrategy::NewRevision,
                    ),
                ));
            }
            InferenceStatus::Unavailable => {
                return Err(ShenkoError::other(
                    "MODEL_PROVIDER_UNAVAILABLE: no provider was dispatched",
                ));
            }
            InferenceStatus::Success
                if result.termination.kind
                    == shenko_broker::request::TerminationKind::ProviderTerminal => {}
            InferenceStatus::Success => {
                return Err(ShenkoError::conflict(
                    "successful model result lacks provider terminal evidence",
                ));
            }
            _ => {
                return Err(ShenkoError::other(
                    "model outcome not conclusive; preserve original awaiting_model for reconciliation",
                ));
            }
        }
        // The response proved provider-terminal: settle this send's own
        // bound attempt rows with what the provider actually reported.
        if let Some(send_id) = claimed_send {
            settle_quota_terminal(&store, &result, request_id, send_id);
        }
        if !result.effective.matches_seal(
            input
                .provider_binding
                .as_ref()
                .expect("broker binding checked"),
            &input.payload,
        ) {
            return Err(ShenkoError::Protocol(
                shenko_types::error::ProtocolError::new(
                    "MODEL_PROVIDER_BINDING_MISMATCH",
                    shenko_types::error::ErrorScope::Request,
                    shenko_types::error::RetryStrategy::NewRevision,
                ),
            ));
        }
        let response = result
            .response
            .ok_or_else(|| ShenkoError::other("successful model result missing response"))?;
        let output = response.content;
        step.model_result_ref = Some(seal_step_object(
            &store,
            seq,
            output.as_bytes(),
            "text/plain",
            "model_results",
            "text",
            1,
        )?);
        let outcome = shenko_types::subject::SubjectModelOutcome {
            schema_version: 2,
            request_id,
            result_ref: step.model_result_ref.clone().unwrap(),
            stop_reason: response.stop_reason.clone(),
            raw_response: response.raw_response,
            usage: response
                .usage
                .as_ref()
                .map(serde_json::to_value)
                .transpose()?,
        };
        step.model_outcome_ref = Some(seal_step_object(
            &store,
            seq,
            &serde_json::to_vec(&outcome)?,
            "application/json",
            "model_outcomes",
            "SubjectModelOutcome",
            2,
        )?);
        step.state = SubjectStepState::ResultRecorded;
        let expected = step.record_revision;
        step.record_revision = RecordRevision(expected.0 + 1);
        store.upsert_step(&step, &state.head, expected)?;
        state.step = step.clone();
        (Some(output), response.stop_reason)
    };

    // Hook: PostModel — SPEC §37. Fires after inference completes, before S1 commit.
    if let Some(reg) = &hooks_ref {
        let dispatch = reg
            .run_phase(&HookPayload::AfterInference {
                request_id: step_id,
                stop_reason: stop_reason.clone(),
                output_tokens: model_output.as_ref().map(|o| o.len() as u64).unwrap_or(0),
            })
            .await;
        debug!(seq, hooks=%dispatch.results.len(), graph=%dispatch.graph_digest, "PostModel hooks ran");
        if dispatch.stopped_early {
            // A stop_unit dispatch before S1 means the unit is not allowed:
            // no decision seal, commit or publication follows. The durable
            // ResultRecorded step keeps its terminal evidence resumable.
            return Err(ShenkoError::conflict(
                "post-inference hook dispatch stopped the unit; recorded result remains resumable",
            ));
        }
    }

    // S1 decision — SPEC §41.5: commit decision + event disposition +
    // delegation intent + unique successor before the ledger write.
    let complete = matches!(stop_reason.as_str(), "stop" | "end_turn" | "stop_sequence");
    let output = if complete {
        crate::output::parse_output(
            step_id,
            step.model_result_ref
                .clone()
                .ok_or_else(|| ShenkoError::conflict("missing model result"))?,
            model_output.as_deref().unwrap_or(""),
            &state.identity_service,
            input.semantic_wait.as_ref().map(|wait| wait.wait_id),
        )
    } else {
        Err(ShenkoError::Protocol(shenko_types::error::ProtocolError::new(
            "MODEL_RESULT_INCOMPLETE", shenko_types::error::ErrorScope::Request,
            shenko_types::error::RetryStrategy::NewRevision,
        ).with_details(serde_json::json!({"stop_reason":stop_reason,"reason":"partial/refused/unknown result is not an action decision"}))))
    };
    // Bind authored model selections to THIS S0, before any S1 action can be
    // accepted. Invalid budget/transport selections use the same sourced R0
    // correction path as invalid controls, never an endless recorded-result loop.
    let parsed = output.and_then(|output| {
        let mut delegations = Vec::new();
        for control in &output.controls {
            let intent = match &control.action {
                OutputAction::Delegate { text, definition } => Some(DelegationIntent {
                    intent_id: control.action_id, goal: text.clone(), system_prompt: text.clone(),
                    model: None, capabilities: vec![], budget_hint: None, reply_grant: None,
                    definition: definition.clone(),
                }),
                OutputAction::DelegateModel { spec } => {
                    let prepared = input.provider_binding.as_ref()
                        .ok_or_else(|| anyhow::anyhow!("model child requires a sealed provider transport"))
                        .and_then(|binding| crate::model_child::prepare_model_delegation(
                            control.action_id, step_id, spec, binding,
                            input.payload.max_tokens.unwrap_or_else(|| binding.default_max_tokens()),
                            &step.software_generation,
                        ));
                    Some(prepared.map_err(|error| ShenkoError::Protocol(
                        shenko_types::error::ProtocolError::new(
                            "CONTROL_PROTOCOL_INVALID", shenko_types::error::ErrorScope::Request,
                            shenko_types::error::RetryStrategy::NewRevision,
                        ).with_details(serde_json::json!({"line":control.spans.first().map(|span|span.line),"reason":error.to_string()}))
                    ))?)
                }
                _ => None,
            };
            if let Some(intent) = intent { delegations.push(intent); }
        }
        Ok((output, delegations))
    });
    let (output, decision_delegation_intents) = match parsed {
        Ok(parsed) => parsed,
        Err(ShenkoError::Protocol(error))
            if matches!(
                error.code.as_str(),
                "CONTROL_PROTOCOL_INVALID" | "MODEL_RESULT_INCOMPLETE"
            ) =>
        {
            replace_rejected_output(state, input, model_output.as_deref().unwrap_or(""), error)?;
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    // ResultRecorded (including real CAS bytes) is already durable before hooks/S1.
    step.state = SubjectStepState::Committed;
    step.record_revision = RecordRevision(step.record_revision.0 + 1);

    // Hook: PreEffect — SPEC §37. Fires before S1 commit is written to the ledger.
    if let Some(reg) = &hooks_ref {
        let dispatch = reg
            .run_phase(&HookPayload::BeforeOperation {
                operation_id: step_id,
                operation_name: format!("commit_step/{seq}"),
            })
            .await;
        debug!(seq, hooks=%dispatch.results.len(), graph=%dispatch.graph_digest, "PreEffect hooks ran");
        if dispatch.stopped_early {
            // A stop_unit dispatch before the S1 commit means the unit is not
            // allowed: no decision intents, head advance or outbox message.
            // The durable ResultRecorded step stays resumable.
            return Err(ShenkoError::conflict(
                "pre-commit hook dispatch stopped the unit; recorded result remains resumable",
            ));
        }
    }

    let mut memory_candidates = vec![];
    let mut publication_intents = vec![];
    let mut policy_intents = vec![];
    let mut identity_changes = vec![];
    let mut identity_action_ids = vec![];
    for control in &output.controls {
        match &control.action {
            OutputAction::Remember { text } => {
                let content_ref = seal_step_object(
                    &store,
                    seq,
                    text.as_bytes(),
                    "text/plain",
                    "memory_intents",
                    "text",
                    1,
                )?;
                memory_candidates.push((
                    MemoryIntent {
                        intent_id: control.action_id,
                        kind: MemoryIntentKind::Save,
                        scope: "personal".into(),
                        content_ref: Some(content_ref),
                    },
                    text.clone(),
                ));
            }
            OutputAction::Reply { target, text } if !text.trim().is_empty() => {
                publication_intents.push(shenko_types::subject::PublicationIntent {
                    intent_id: control.action_id,
                    target: target.clone(),
                    content: text.clone(),
                    reply_grant_id: None,
                });
            }
            OutputAction::Identity { change } => {
                identity_changes.push(change.clone());
                identity_action_ids.push(control.action_id);
            }
            OutputAction::SemanticWait { change } => {
                policy_intents.push(PolicyIntent {
                    intent_id: control.action_id,
                    kind: PolicyKind::SemanticWait { change: change.clone() },
                    description: "subject semantic wait".into(),
                    target_revision: None,
                });
            }
            _ => {}
        }
    }
    let identity_intent = if identity_changes.is_empty() {
        None
    } else {
        let current = input_identity.as_ref().ok_or_else(|| {
            ShenkoError::conflict("identity edit requires initialized S0 profile")
        })?;
        state
            .identity_service
            .revise(
                current,
                &identity_changes,
                IdentitySource::Subject {
                    step_id,
                    step_no: seq,
                },
            )
            .map(|proposed| IdentityIntent {
                intent_id: identity_action_ids[0],
                action_ids: identity_action_ids,
                expected_revision: current.revision,
                provider_revision: state.identity_service.revision().into(),
                proposed,
            })
    };

    let event_dispositions: Vec<EventDisposition> = events
        .iter()
        .map(|ev| EventDisposition {
            event_id: ev.event_id.clone(),
            disposition: Disposition::Acknowledged,
        })
        .collect();

    // 决定对象没有自引用；后继身份在 CAS 决定与数据库事务中保持一致。
    let decision = StepDecision {
        schema_version: 7,
        step_id,
        output: output.clone(),
        identity_intent,
        input_plan_ref: step
            .input_plan_ref
            .clone()
            .ok_or_else(|| ShenkoError::conflict("missing input plan"))?,
        model_result_ref: step.model_result_ref.clone(),
        applied_steering_refs: vec![],
        event_dispositions,
        delegated_task_intents: decision_delegation_intents.clone(),
        child_dialogue_intents: vec![],
        memory_intents: memory_candidates.iter().map(|(i, _)| i.clone()).collect(),
        policy_intents,
        publication_intents: publication_intents.clone(),
        pending_obligations: vec![],
        evidence_refs: vec![],
        successor_step_id: next_step_id,
    };
    let decision_bytes = serde_json::to_vec(&decision)?;
    step.decision_ref = Some(seal_step_object(
        &store,
        seq,
        &decision_bytes,
        "application/json",
        "decisions",
        "StepDecision",
        7,
    )?);

    match store.commit_step_and_advance_head(&step, &state.head, next_step_id) {
        Ok(receipt) => {
            // 只在提交明确成功后采用内存状态并发布。
            let (head, successor) = store.load_subject(state.agent_id)?;
            state.head = head;
            state.step = successor;
            state.previous_input_tokens = observed_input_tokens(&store.recorded_model_outcome(&step)?, input.provider_binding.as_ref());
            state.previous_input_binding = input.provider_binding.clone();
            state.previous_estimated_input_tokens = input.context.estimated_input_tokens;
            state.semantic_wait = store.semantic_wait(state.agent_id)?;
            state.semantic_wait_receipt = store.semantic_wait_receipt(step_id)?;

            // Read our committed receipt, not latest profile. Its revision and
            // typed conflict were settled atomically with this exact S1.
            let identity_receipt = store
                .identity_receipt(step_id)?
                .ok_or_else(|| ShenkoError::conflict("S1 identity receipt missing"))?;
            let identity_revision = identity_receipt.outcome.revision();
            state.identity_receipt = Some(identity_receipt);

            // Outbound path — publish ONLY after the S1 commit succeeded.
            // SPEC §41.2.3 committed-only publication: never show a thought
            // that is not durably recorded, or a retried step could show a
            // different one.
            if model_output.is_some() {
                if let Some(tx) = &state.outbox_tx {
                    let _ = tx.send(OutboxMessage {
                        kind: "thought".into(),
                        step: seq,
                        step_id,
                        commit_ref: receipt.commit_ref.clone(),
                        replay_seq: receipt.commit_ref.seq.0,
                        text: decision.output.text.clone(),
                        publications: publication_intents.clone(),
                        identity_revision,
                    });
                }
            }

            if seq % 50 == 0 || !events.is_empty() {
                debug!(
                    seq,
                    events=%events.len(),
                    in_wait=in_semantic_wait,
                    has_model=model_output.is_some(),
                    commit=%receipt.commit_ref,
                    "S1 committed"
                );
            }

            // Hook: PostEffect — SPEC §37. Fires after S1 is durably committed.
            if let Some(reg) = &hooks_ref {
                let dispatch = reg
                    .run_phase(&HookPayload::AfterOperation {
                        operation_id: step_id,
                        outcome: "committed".to_string(),
                        effect_status: format!("commit/{seq}"),
                    })
                    .await;
                debug!(seq, hooks=%dispatch.results.len(), graph=%dispatch.graph_digest, "PostEffect hooks ran");
            }

            Ok(Some(receipt.commit_ref))
        }
        Err(e) => {
            warn!(seq, err=%e, "S1 commit failed");
            Err(e)
        }
    }
}

/// A protocol rejection is conclusive, unlike an unknown provider outcome.
/// R0 records it and freezes a new correction request before adopting memory.
fn replace_rejected_output(
    state: &mut SubjectLoopState,
    mut input: FrozenStepInput,
    raw_output: &str,
    error: shenko_types::error::ProtocolError,
) -> Result<(), ShenkoError> {
    use shenko_types::subject::ControlOutputRejection;
    let store = &state.store;
    let mut retired = state.step.clone();
    let raw_ref = retired
        .model_result_ref
        .clone()
        .ok_or_else(|| ShenkoError::conflict("rejection missing recorded result"))?;
    let replacement_id = EntityId::new();
    let rejection = ControlOutputRejection {
        schema_version: 2,
        supersedes_step_id: retired.step_id,
        replacement_step_id: replacement_id,
        request_id: retired
            .active_request_id
            .ok_or_else(|| ShenkoError::conflict("rejection missing request"))?,
        input_plan_ref: retired
            .input_plan_ref
            .clone()
            .ok_or_else(|| ShenkoError::conflict("rejection missing input plan"))?,
        model_result_ref: raw_ref.clone(),
        model_outcome_ref: retired
            .model_outcome_ref
            .clone()
            .ok_or_else(|| ShenkoError::conflict("rejection missing terminal outcome"))?,
        error,
    };
    let reason_ref = seal_step_object(
        store,
        retired.step_no,
        &serde_json::to_vec(&rejection)?,
        "application/json",
        "control_rejections",
        "ControlOutputRejection",
        2,
    )?;
    // Preserve the selected events and pinned persona/provider. This is a new
    // step/request seal with explicit feedback, not replay of the invalid wire.
    input.step_id = replacement_id;
    input
        .payload
        .messages
        .push(shenko_broker::request::InferenceMessage::text(
            "assistant",
            raw_output,
        ));
    input.payload.messages.push(shenko_broker::request::InferenceMessage::text(
        "user",
        shenko_store::control_rejection_feedback(&rejection),
    ));
    let mut replacement = retired.clone();
    replacement.step_id = replacement_id;
    replacement.state = SubjectStepState::Preparing;
    replacement.input_plan_ref = Some(seal_step_object(
        store,
        retired.step_no,
        &serde_json::to_vec(&input)?,
        "application/json",
        "input_plans",
        "subject_step_input",
        11,
    )?);
    replacement.context_revision = Some(
        retired
            .context_revision
            .and_then(|revision| revision.checked_add(1))
            .ok_or_else(|| {
                ShenkoError::conflict("correction context revision exhausted/missing")
            })?,
    );
    replacement.active_request_id = None;
    replacement.active_attempt_id = None;
    replacement.model_result_ref = None;
    replacement.model_outcome_ref = None;
    replacement.decision_ref = None;
    replacement.commit_ref = None;
    replacement.retry_reason_ref = Some(reason_ref.clone());
    replacement.record_revision = RecordRevision(0);
    retired.state = SubjectStepState::Retired;
    retired.retry_reason_ref = Some(reason_ref);
    retired.record_revision = RecordRevision(retired.record_revision.0 + 1);
    let receipt = store.reject_output_and_replace_step(&retired, &state.head, &replacement)?;
    let (head, step) = store.load_subject(state.agent_id)?;
    state.head = head;
    state.step = step;
    warn!(step_id=%retired.step_id, replacement_step_id=%replacement_id,
        commit=%receipt.commit_ref, error=%rejection.error, "control result rejected; correction registered");
    Ok(())
}

fn record_retry(state: &mut SubjectLoopState, fact: InferenceRetryFact) -> Result<(), ShenkoError> {
    let request_id = state
        .step
        .active_request_id
        .ok_or_else(|| ShenkoError::conflict("missing retry request"))?;
    let evidence = SubjectRetryEvidence { request_id, fact };
    let mut step = state.step.clone();
    step.retry_reason_ref = Some(seal_step_object(
        &state.store,
        step.step_no,
        &serde_json::to_vec(&evidence)?,
        "application/json",
        "retry_evidence",
        "subject_retry_evidence",
        1,
    )?);
    step.state = SubjectStepState::RetryPending;
    let expected = step.record_revision;
    step.record_revision = RecordRevision(expected.0 + 1);
    state.store.upsert_step(&step, &state.head, expected)?;
    state.step = step;
    Ok(())
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod recovery_tests;
