//! Continuous subject loop — SPEC §5.1, §41.5
//! Real S0/S1 step state machine. Never terminates on empty inbox.
//! Semantic wait hard-max 600s. Yields inference slot after each step.
use crate::inbox::{Inbox, InboxEvent};
use crate::context_projection::{ContextPreparer, FrozenContextInput, estimate_payload_tokens, prepare_context, wait_for_context_preparer};
use crate::outbox::OutboxMessage;
use crate::quota_gate::{QuotaGate, quota_dispatch_gate, observe_quota_response, settle_quota_terminal};
use shenko_identity::IdentityService;
use shenko_kernel::hooks::{HookPayload, HookRegistry};
use shenko_store::subject_request::{SubjectRequestAttempt, SubjectRequestState};
use shenko_types::commit::CommitRef;
use shenko_types::error::ShenkoError;
use shenko_types::identity::{
    IdentityIntent, IdentityOutcome, IdentityProfile, IdentityReceipt, IdentitySource,
};
use shenko_types::ids::{EntityId, LedgerSeq, RecordRevision};
use shenko_types::object::{Digest, ObjectRef};
use shenko_types::output::OutputAction;
use shenko_types::subject::{
    AttentionPolicy, DelegationIntent, DialogueDirection, DialogueIntent, Disposition,
    EventDisposition, InferenceRetryFact, MemoryIntent, MemoryIntentKind, PendingObligation,
    PolicyIntent, PolicyKind, SemanticWait, SemanticWaitReceipt, SemanticWaitOutcome,
    StepDecision, SubjectBranchHead, SubjectRetryEvidence, SubjectStep, SubjectMode,
    SubjectStepState,
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
    /// The wire request identity of this step's FIRST attempt. It is sealed
    /// before any send (SPEC §41.5 S0), so a resumed step reuses the sealed
    /// identity instead of minting a fresh one for the same sealed request.
    request_id: EntityId,
    /// The hook graph this step's dispatch is bound to. Without a sealed
    /// graph the step cannot be admitted for dispatch: an absent hook runtime
    /// is a typed error, never a fabricated empty graph.
    hook_graph_ref: Option<ObjectRef>,
    /// The RESOLVED provider selection this request is sealed against. The
    /// selection is made BEFORE the seal: the plan states the exact
    /// account/profile/model the request will use and the revision of the
    /// binding that produced it, so a "pick another model" decision can
    /// never be applied after the request was sealed.
    provider_selection: ProviderSelection,
    /// The prepared contributions collected for this step. Their provenance
    /// (producer, version, artifact digest) reaches the raw call record
    /// through this seal.
    contributions: Vec<serde_json::Value>,
    /// Why each prepared contribution was not sent. An omission is data.
    contribution_omissions: Vec<serde_json::Value>,
    /// The token cost the contributions charged against this step's budget.
    contribution_tokens: u64,
    /// The durable obligations the subject is still carrying. They are read
    /// back into every later S0 until they are completed or cancelled.
    obligations: Vec<PendingObligation>,
    /// External effects whose outcome is still unknown, each with the case
    /// the subject may explicitly accept for now (SPEC §41.4 X11).
    ///
    /// It is read at S0 and sealed with the request, so an acknowledgement at
    /// S1 can only name a case this step was actually shown. Absent in
    /// schema-12 plans, which simply carried no docket.
    #[serde(default)]
    reconciliation_docket: Vec<shenko_store::ReconciliationDocketEntry>,
    /// The subject's own attention (免打扰) decision in force for this step.
    attention: Option<AttentionPolicy>,
}

/// The resolved provider identity frozen with the request. Every field is
/// read from the binding itself, so an open profile binding reports its own
/// account, profile and transport revision instead of a re-derived guess.
#[derive(serde::Serialize, serde::Deserialize)]
struct ProviderSelection {
    schema_version: u32,
    binding: Option<shenko_broker::request::ProviderBinding>,
    model: Option<String>,
    profile_id: Option<String>,
    profile_revision: Option<u32>,
    credential_fingerprint: Option<String>,
    resource_domain_id: Option<String>,
    serializer_revision: Option<u32>,
}

/// Freeze the resolved provider selection. An absent broker yields an empty
/// selection: the step then has no transport and the dispatch refuses it —
/// it never invents one.
fn provider_selection(
    binding: Option<&shenko_broker::request::ProviderBinding>,
) -> ProviderSelection {
    let Some(binding) = binding else {
        return ProviderSelection {
            schema_version: 1,
            binding: None,
            model: None,
 profile_id: None,
            profile_revision: None,
 credential_fingerprint: None,
            resource_domain_id: None,
 serializer_revision: None,
        };
    };
    let json = serde_json::to_value(binding).unwrap_or(serde_json::Value::Null);
    ProviderSelection {
        schema_version: 1,
        binding: Some(binding.clone()),
        model: Some(binding.model().to_owned()),
        profile_id: Some(binding.profile_id().to_owned()),
        profile_revision: Some(binding.profile_revision()),
        credential_fingerprint: json
            .get("credential_fingerprint")
            .and_then(|value| value.as_str())
            .map(str::to_owned),
        resource_domain_id: json
            .pointer("/profile/resource_domain_id")
            .and_then(|value| value.as_str())
            .map(str::to_owned),
        serializer_revision: json
            .get("serializer_revision")
            .and_then(|value| value.as_u64())
            .map(|value| value as u32),
}
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
    // 13 adds the reconciliation docket. A schema-12 plan is still the same
    // sealed request; it simply carried no case the subject could accept, so
    // an acknowledgement naming a case at 12 is refused (SPEC §51.1: old
    // bytes keep their old meaning).
    if !matches!(input.schema_version, 12 | 13)
        || input.step_id != step.step_id
        || input.step_no != step.step_no
    {
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
        AwaitingModel => {
            // A resumed step consults the durable receipts of its OWN
            // attempts. The terminal evidence decides whether the result
            // already exists, an unknown outcome is reconciled instead of
            // replayed, and a provably never-dispatched request may be armed
            // again under the same sealed identity. Nothing here starts a
            // second send on its own.
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
    /// Registered IM channel plugins: the reply grammar, reply-target
    /// resolution and ingress staging all bind through this boundary.
    pub channels: shenko_types::channel::ChannelRegistry,
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
    pub(super) context_preparer: Option<ContextPreparer>,
    /// Pinned for one step, after replaying all accepted S0 usage into this
    /// service instance. A hot replacement must replay before collection.
    pub(super) contribution_registry: Option<Arc<shenko_context_policy::ContributionRegistry>>,
    native_capability_sections: Vec<String>,
    /// Rolling goals the subject is pursuing.
    pub goals: Vec<String>,
    /// External effects whose outcome is still unknown, as read at this
    /// step's S0. They are shown to the model and sealed with the request;
    /// nothing here resolves an effect or resends one (SPEC §41.4 X11).
    pub reconciliation_docket: Vec<shenko_store::ReconciliationDocketEntry>,
    /// Outbound path for completed model output — SPEC §5.1, §21.12.
    pub outbox_tx: Option<tokio::sync::broadcast::Sender<OutboxMessage>>,
    /// Identity revision fixed for the current step — SPEC §6. Reloaded only
    /// at the S0 boundary, so a change applies from the next step on.
    pub identity: Option<IdentityProfile>,
    pub identity_receipt: Option<IdentityReceipt>,
    pub(super) plugin_context: cordis_core::Context,
    identity_service: Arc<IdentityService>,
    /// The cognition catalogue available to this subject. It is a READ MODEL
    /// over the Node's unit store: the Node stays the sole install, version,
    /// enable, supersession and provenance authority. The catalogue only
    /// discloses; its bodies are advisory material for the model, never a
    /// dispatch table.
    pub(super) awareness: Option<Arc<shenko_awareness_core::CognitionCatalog>>,
    /// The installed quota strategy set (SPEC §42.1).
    pub(super) quota_strategies: shenko_quota::QuotaStrategies,
    /// The notebook reminder this step's assembled request carries.
    pub(super) pending_reminder: Option<shenko_context_policy::RolloverReminder>,
}

impl SubjectLoopState {
    /// 从真实 head/唯一后继恢复；未知模型状态不允许重新发起推理。
    pub fn new(
        agent_id: EntityId,
        inbox: Arc<Inbox>,
        store: Arc<shenko_store::store::Store>,
        plugin_context: cordis_core::Context,
        channels: shenko_types::channel::ChannelRegistry,
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
        let quota_strategies = crate::quota_strategies::installed(&plugin_context);
        let mut state = Self {
            agent_id,
            head,
            step,
            inbox,
            channels,
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
            context_preparer: None,
            contribution_registry: None,
            native_capability_sections: vec![],
            goals: vec![],
            reconciliation_docket: vec![],
            outbox_tx: None,
            identity,
            identity_receipt,
            plugin_context,
            identity_service,
            awareness: None,
            quota_strategies,
            pending_reminder: None,
        };
        state.refresh_contributions()?;
        Ok(state)
    }

    fn refresh_contributions(&mut self) -> Result<(), ShenkoError> {
        let current = self.plugin_context
            .try_service::<shenko_context_policy::ContributionRegistry>().ok();
        if let Some(registry) = &current {
            if self.contribution_registry.as_ref().is_some_and(|old| Arc::ptr_eq(old, registry)) {
                return Ok(());
            }
            for step in self.store.sealed_subject_steps(&self.head.branch_id)? {
                let input = frozen_input(&self.store, &step)?;
                let seal = step.input_plan_ref.as_ref().expect("sealed step selected");
                if let Some(usage) = contribution_usage(&input, seal)? {
                    registry.record_usage(usage).map_err(|error| ShenkoError::conflict(error.to_string()))?;
                }
            }
        }
        self.contribution_registry = current;
        Ok(())
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

    /// Attach the cognition catalog. Every installed unit's card travels as
    /// advisory material for the step; the model decides what it means here, so
    /// no kernel branch is created per topic and no unit body is injected
    /// without the model asking for it at an exact version.
    pub fn with_awareness(
        mut self,
        catalog: Arc<shenko_awareness_core::CognitionCatalog>,
    ) -> Self {
        self.awareness = Some(catalog);
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
    // Admitted inputs this process could not decode. They stay durable and
    // unconsumed; this only remembers them so one bad row is journaled once
    // per open instead of on every step.
    let mut undecodable_inputs: std::collections::HashSet<shenko_types::ids::EntityId> =
        std::collections::HashSet::new();

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
            // The whole per-step budget (§41.6: at most 64 events) is read
            // from the wake channel in one drain, so a burst never leaves
            // the dispatcher blocked on this consumer's bounded buffer.
            state.inbox.recv_batch(MAX_STEP_EVENTS, 80).await
        } else {
            // Never consume new input into a recovered, already frozen step.
            vec![]
        };
        // Drain durable input without sending into this consumer's own bounded
        // channel. A backlog larger than inbox capacity must not block recovery.
        if state.step.state == SubjectStepState::Planned {
            // §41.6: at most 64 events are selected into one step; the durable
            // drain only fills the remainder after the live batch.
            let drain_limit = MAX_STEP_EVENTS.saturating_sub(events.len());
            // Rows this process already proved undecodable keep their position
            // in the bounded page; ask past them so they cannot starve readable
            // inputs. The bound keeps one pathological queue from widening the
            // read without limit.
            let durable_limit = drain_limit
                .saturating_add(undecodable_inputs.len())
                .min(MAX_STEP_EVENTS * 16);
            match if drain_limit == 0 {
                Ok(vec![])
            } else {
                state
                    .store
                    .pending_inbox_events(state.agent_id, durable_limit)
            } {
                Ok(rows) => {
                    if !rows.is_empty() {
                        debug!(count = rows.len(), "draining durable web inputs");
                    }
                    for (event_id, data) in rows {
                        // The widened read exists to see past undecodable rows;
                        // the selection budget itself is unchanged.
                        if events.len() >= MAX_STEP_EVENTS {
                            break;
                        }
                        // One admitted row this binary cannot decode — a newer
                        // event kind after a downgrade, or a damaged payload —
                        // is an undecodable FACT, not a licence to stop the
                        // subject (SPEC §41.4 X11). The original bytes stay
                        // durable and unconsumed, the skip is journaled as a
                        // bounded runtime fact, and unrelated work continues; a
                        // repaired reader picks the row up after the next open.
                        let event = match serde_json::from_str::<InboxEvent>(&data) {
                            Ok(event) => event,
                            Err(error) => {
                                if undecodable_inputs.insert(event_id) {
                                    state.store.journal_runtime_error(
                                        state.agent_id,
                                        shenko_store::RuntimeErrorContext {
                                            kind: "undecodable_inbox_input".into(),
                                            source_module: module_path!().into(),
                                            message: format!(
                                                "admitted inbox row {event_id} is not an InboxEvent this build reads: {error}"
                                            ),
                                            step_no: Some(seq),
                                            task_id: None,
                                            extra: serde_json::json!({
                                                "event_id": event_id.to_string(),
                                            }),
                                        },
                                    );
                                }
                                continue;
                            }
                        };
                        // A live wake may carry the same event. The persisted
                        // original owns its identity and payload, never the
                        // duplicate copy delivered by the volatile channel.
                        if let Some(existing) = events.iter_mut()
                            .find(|existing| existing.event_id == event.event_id)
                        {
                            *existing = event;
                        } else {
                            events.push(event);
                        }
                    }
                }
                Err(error) => break Err(error),
            }
        }
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

    // Every error that stops the loop is a durable runtime fact the subject
    // can later read through errors.read — SPEC §45.14. The journal write is
    // fail-soft: it never masks or replaces the original error.
    if let Err(error) = &result {
        state.store.journal_runtime_error(
            state.agent_id,
            shenko_store::RuntimeErrorContext {
                kind: "subject_loop".into(),
                source_module: module_path!().into(),
                message: format!("subject loop stopped on error: {error}"),
                step_no: Some(state.step.step_no),
                task_id: None,
                extra: serde_json::json!({}),
            },
        );
    }
    // Keeping Arc<Inbox> in main must not leave a dead consumer accepting input.
    state.inbox.close().await;
    info!("subject loop terminated");
    result
}

/// The engineering default of §41.6: one subject step selects at most this
/// many input events (live wake and durable drain together).
const MAX_STEP_EVENTS: usize = 64;

/// Role and output contract of the subject. It names the framework, never the
/// instance, and does not vary with identity revisions (R054). SPEC §5.1, §6.
pub(crate) const SUBJECT_CONTRACT: &str = "你是一个持续运行的个人 agent，运行在 Shenko 框架上。\n\
     你有持续的内心生活：关注、兴趣、承诺、疑问。\n\
     你在没有外部消息时也继续思考——空闲不是停止的理由。\n\
     思考是连续的，上一步的想法会成为这一步的起点。\n\
     普通正文是自己的想法，可显示在系统时间线，但不会作为外部消息发送。要让对方收到的话，必须通过回复控制行发送。";

/// System prompt for the continuous subject: contract → persona → per-step
/// suffix. SPEC §5.1, §6 ordering; §13.5 keeps the stable part first.
/// Stable prefix: role contract, the installed native definitions and the
/// persona. SPEC §13.5 keeps it first so the model/template binding stays
/// reusable across steps; the context profile digest covers this text, not the
/// per-step suffix.
pub(super) fn stable_system_prompt(state: &SubjectLoopState) -> String {
    let mut prompt = crate::canonical_system_prompt(&state.channels);

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
    obligations: &[PendingObligation],
) -> String {
    let mut prompt = String::new();

    if !awareness.is_empty() {
        prompt.push_str(
            "\n\n已安装的认知单元卡片（给你一些视角，不是命令；哪些此刻有用由你判断，可以采纳、改写或忽略。需要正文时按卡片给出的 unit_id 与 revision 用 cognition.unit.read 读取确切版本，正文不会自动注入）：\n",
        );
        for (index, body) in awareness.iter().enumerate() {
            prompt.push_str(&(index + 1).to_string());
            prompt.push_str(". ");
            prompt.push_str(body.trim());
            prompt.push('\n');
        }
    }

    if !obligations.is_empty() {
        prompt.push_str("\n\n当前仍未结束的持久义务（本轮冻结事实；完成或取消须显式给出终态和理由）：\n");
        prompt.push_str(&serde_json::to_string(obligations).expect("obligation projection serializes"));
        prompt.push(char::from(10));
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

    // Once per committed window, when the observed pressure crosses the
    // installed notes strategy's own threshold. This is a capability
    // reminder, not a required procedure: it never cuts a window by itself.
    // It is the reminder the S0 projection read for THIS request, so the
    // prompt and the answer recorded at commit name the same fact.
    if state.pending_reminder.is_some() {
        prompt.push_str("\n\n");
        prompt.push_str(shenko_context_policy::ROLLOVER_REMINDER);
        prompt.push('\n');
    }
    if !state.reconciliation_docket.is_empty() {
        // These are durable facts about effects nobody has proven either way.
        // They are not a queue and not a suggestion: the subject may accept
        // one, or leave it open.
        prompt.push_str(
            "\n\n外部效果仍然未知的持久事实（既不是成功也不是失败；不会自动重发）：\n",
        );
        for entry in &state.reconciliation_docket {
            let view = serde_json::json!({
                "case_id": entry.case.case_id,
                "operation": entry.operation_name,
                "operation_state": entry.operation_state,
                "effect_status": entry.effect_status,
                "reason": entry.case.reason,
                "evidence_refs": entry.case.evidence_refs,
            });
            prompt.push_str("- ");
            prompt.push_str(&serde_json::to_string(&view).expect("docket view serializes"));
            prompt.push('\n');
        }
        prompt.push_str(
            "若你判断可以带着这份不确定性继续别的工作，用 [reconcile] {\"case_id\":\"上面某个 case_id\",\"reason_refs\":[\"该 case 的 evidence_refs 之一\"],\"remaining_impact\":\"剩下的影响与还需什么证据\"} 明确接受。它只记录你的判断，不会把效果改成成功，也不会重发；以后收到确定证据会重新评估。\n",
        );
    }

    prompt
}

pub(super) fn build_system_prompt(
    state: &SubjectLoopState,
    awareness: &[String],
    obligations: &[PendingObligation],
) -> String {
    let mut prompt = stable_system_prompt(state);
    prompt.push_str(&system_prompt_suffix(state, awareness, obligations));
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
        // An installed profile names its own usage fields; this projection
        // only reads the historical local/Anthropic shapes, so a profile
        // binding stays unknown here instead of guessing a field.
        ProviderBinding::Profile { .. } => return None,
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
    if !matches!(
        input.get("schema_version").and_then(serde_json::Value::as_u64),
        Some(12) | Some(13)
    ) {
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
    state.refresh_contributions()?;
    let store = Arc::clone(&state.store);
    // Pin one graph through all phases of this invocation. A hot swap must not
    // combine an old BeforeInference with a new AfterInference handler set.
    // The registry comes from the plugin runtime when it publishes one; an
    // absent runtime is a typed refusal at S0, never a silently empty graph.
    let hook_registry = state
        .hooks
        .clone()
        .or_else(|| state.plugin_context.try_service::<HookRegistry>().ok());
    let hooks_ref = hook_registry.as_ref().map(|registry| registry.snapshot());

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
        let identity_service = state
            .plugin_context
            .try_service::<IdentityService>()
            .map_err(|e| ShenkoError::other(format!("IDENTITY_PROVIDER_UNAVAILABLE: {e}")))?;
        state.identity_service = identity_service;
        state.identity = store.latest_identity(state.agent_id)?;
        // The hook graph this step dispatches under is sealed together with
        // the request. An absent hook runtime is a typed refusal: an empty
        // graph would pretend that hooks ran.
        let hook_graph_ref = seal_hook_graph(&store, seq, hook_registry.as_ref())?;
        // The provider selection is resolved BEFORE the seal: the request
        // states the exact account/profile/model it will use, and the
        // dispatch still compares the sealed binding with the live broker
        // binding, so a post-seal change is refused instead of applied.
        let input_binding = state.broker.as_ref().map(|broker| broker.binding().clone());
        let input_provider_selection = provider_selection(input_binding.as_ref());
        // Durable obligations and the attention decision travel with the
        // request: a commitment the subject already took is part of what it
        // is thinking about, not a note in a later prompt.
        let attention = store.current_subject_attention(state.agent_id)?;
        // External effects whose outcome is still unknown travel with the
        // request too: the subject cannot accept an uncertainty it was never
        // shown, and the sealed docket is what S1 checks the acceptance
        // against. The read is bounded; nothing here resends or resolves an
        // effect.
        let reconciliation_docket = store.open_reconciliation_docket(state.agent_id, 8)?;
        state.reconciliation_docket = reconciliation_docket.clone();
        let prepared = match prepare_context(state, events) {
            Ok(Some(prepared)) => prepared,
            Ok(None) => {
                wait_for_context_preparer(state, cancel).await;
                return Ok(None);
            }
            Err(ShenkoError::ResnapshotRequired { reason }) => {
                // No S0 seal or model admission exists. The outer loop yields
                // and rebuilds this same Planned step from durable staging.
                tracing::debug!(seq, %reason, "S0 source advanced; reprojecting planned step");
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let input = FrozenStepInput {
            schema_version: 13,
            step_id,
            step_no: seq,
            events: prepared.events,
            provider_binding: input_binding,
            payload: prepared.payload,
            context: prepared.context,
            semantic_wait: prepared.semantic_wait,
            identity: state.identity.clone(),
            identity_provider_revision: state.identity_service.revision().into(),
            request_id: EntityId::new(),
            hook_graph_ref: Some(hook_graph_ref),
            provider_selection: input_provider_selection,
            contributions: prepared.contributions,
            contribution_omissions: prepared.contribution_omissions,
            contribution_tokens: prepared.contribution_tokens,
            obligations: prepared.obligations,
            reconciliation_docket,
            attention,
        };
        step.input_plan_ref = Some(seal_step_object(
            &store,
            seq,
            &serde_json::to_vec(&input)?,
            "application/json",
            "input_plans",
            "subject_step_input",
            13,
        )?);
        step.context_revision = Some(seq);
        step.state = SubjectStepState::Preparing;
        let expected = step.record_revision;
        step.record_revision = RecordRevision(expected.0 + 1);
        match commit_s0(state, step.clone(), &input) {
            Ok(_) => {}
            Err(ShenkoError::ResnapshotRequired { reason }) => {
                tracing::debug!(seq, %reason, "S0 CAS rejected stale source; reprojecting planned step");
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
        input
    } else {
        resumed_input.ok_or_else(|| ShenkoError::conflict("missing validated S0"))?
    };
    let events = input.events.as_slice();
    let in_semantic_wait = input.semantic_wait.as_ref().is_some_and(SemanticWait::is_active);
    let input_identity = input.identity.clone();
    // The graph that dispatches must be the graph the request was sealed
    // under. A hot swap between S0 and the send is a typed conflict, not a
    // silent re-run under a new graph.
    if let Some(sealed_ref) = input.hook_graph_ref.as_ref() {
        let registry = hook_registry.as_ref().ok_or_else(|| {
            ShenkoError::other(
                "HOOK_RUNTIME_UNAVAILABLE: the sealed hook graph cannot be honoured; no dispatch is admitted",
            )
        })?;
        let sealed: serde_json::Value = serde_json::from_slice(&read_sealed(&store, sealed_ref)?)?;
        let sealed_digest = sealed
            .get("graph_digest")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let pinned = hooks_ref
            .as_ref()
            .map(|graph| graph.graph_digest().0.clone())
            .unwrap_or_default();
        let _ = registry;
        if sealed_digest != pinned {
            return Err(ShenkoError::conflict(
                "the sealed hook graph changed before dispatch; re-seal the step explicitly",
            ));
        }
    }
    // A resumed AwaitingModel step consults its OWN durable receipts before
    // anything else: the terminal evidence decides whether the result
    // already exists, an unknown outcome is reconciled instead of replayed,
    // and a provably never-dispatched request is armed again. Nothing here
    // starts a second send on its own.
    let mut recovered: Option<(String, String)> = None;
    if step.state == SubjectStepState::AwaitingModel {
        // Restore the ORIGINAL request identity with the broker before any
        // new admission. A request that may still be in flight keeps its
        // recoverable debt; a request whose terminal receipt already landed
        // is reconciled against that receipt, never re-sent.
        if let (Some(broker), Some(request_id)) =
            (state.broker.as_ref(), step.active_request_id)
        {
            let attempt = store.subject_request_attempt(request_id)?
                .ok_or_else(|| ShenkoError::conflict("awaiting request has no durable attempt"))?;
            // Armed is positive evidence that the send boundary did not pass.
            // Registering it as unknown would consume broker capacity forever.
            if attempt.state != SubjectRequestState::Armed {
            let sink = Arc::new(crate::subject_receipt::SubjectReceiptSink {
                store: Arc::clone(&state.store),
                agent_id: state.agent_id,
                step_id,
                request_id,
                input_plan_ref: step
                    .input_plan_ref
                    .clone()
                    .ok_or_else(|| ShenkoError::conflict("S0 input plan missing"))?,
                payload_digest: Digest::blake3(&serde_json::to_vec(&input.payload)?),
                provider_binding_digest: Digest::blake3(&serde_json::to_vec(
                    &input.provider_binding,
                )?),
                binding: input
                    .provider_binding
                    .clone()
                    .ok_or_else(|| ShenkoError::conflict("S0 provider binding missing"))?,
                payload: input.payload.clone(),
            });
            broker.register_recovered(sink).map_err(|error| {
                ShenkoError::conflict(format!("original request recovery failed: {error:#}"))
            })?;
            }
        }
        match recover_awaiting_model(state, &mut step, &input)? {
            AwaitingRecovery::Adopted { output, stop_reason } => {
                recovered = Some((output, stop_reason));
            }
            AwaitingRecovery::RetryRecorded => return Ok(None),
        }
    }
    let (model_output, stop_reason) = if let Some((output, stop_reason)) = recovered {
        (Some(output), stop_reason)
    } else if step.state == SubjectStepState::ResultRecorded {
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
                    .dispatch(&HookPayload::BeforeInference {
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
                debug!(seq, hooks=%dispatch.results.len(), graph=%reg.graph_digest(), "PreModel hooks ran");
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
        // A retry is a NEW attempt. The previous request identity is never
        // reused, so a spent, rejected or unknown request can never be
        // re-sent under its own name. The first attempt of a step uses the
        // identity sealed in S0.
        let request_id = match step.state {
            SubjectStepState::RetryPending => EntityId::new(),
            _ => step.active_request_id.unwrap_or(input.request_id),
        };
        let claimed_send = match quota_dispatch_gate(
            &store,
            broker.binding(),
            request_id,
            cancel,
            &state.quota_strategies,
            None,
            Some(state.agent_id),
        )
        .await
        {
            Ok(QuotaGate::Ready(send_id)) => send_id,
            Ok(QuotaGate::Cancelled) => return Ok(None),
            Err(error) => return Err(error),
        };
        // Arm the attempt BEFORE any wire send. The request identity, the
        // sealed plan and the payload/binding digests commit first, so a
        // crash after the send still finds the original request.
        let input_plan_ref = step
            .input_plan_ref
            .clone()
            .ok_or_else(|| ShenkoError::conflict("S0 input plan missing"))?;
        let attempt = SubjectRequestAttempt {
            schema_version: 1,
            request_id,
            // Re-arming the SAME request identity keeps its attempt id: a
            // request that provably never dispatched may be armed again,
            // but it is never a second attempt under one identity.
            attempt_id: store
                .subject_request_attempt(request_id)?
                .map(|attempt| attempt.attempt_id)
                .unwrap_or_else(EntityId::new),
            agent_id: state.agent_id,
            step_id,
            send_id: claimed_send,
            state: SubjectRequestState::Armed,
            input_plan_ref,
            payload_digest: Digest::blake3(&serde_json::to_vec(&input.payload)?),
            provider_binding_digest: Digest::blake3(&serde_json::to_vec(&input.provider_binding)?),
            terminal: None,
            created_commit: CommitRef::new(EntityId::nil(), LedgerSeq::ZERO),
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
            // The armed attempt row commits in that same transaction: the
            // request can never be visible without its receipt identity.
            Some(send_id) => store.upsert_step_arming_request(
                &step,
                &state.head,
                expected,
                Some(send_id),
                &attempt,
            )?,
            None => store.upsert_step_arming_request(
                &step,
                &state.head,
                expected,
                None,
                &attempt,
            )?,
        };
        state.step = step.clone();
        // The receipt sink owns the durable send boundary and the terminal
        // provider evidence: both commit independently of whether this
        // waiting step is still alive.
        let sink = Arc::new(crate::subject_receipt::SubjectReceiptSink {
            store: Arc::clone(&state.store),
            agent_id: state.agent_id,
            step_id,
            request_id,
            input_plan_ref: attempt.input_plan_ref.clone(),
            payload_digest: attempt.payload_digest.clone(),
            provider_binding_digest: attempt.provider_binding_digest.clone(),
            binding: broker.binding().clone(),
            payload: input.payload.clone(),
        });
        if let Err(error) = broker.submit_recorded(req, sink) {
            // An admission error is not authority to claim the request never
            // left: the broker owns the send boundary. The request stays
            // recoverable debt and the step is reconciled, never replayed
            // under the same identity.
            broker.recover_unknown(request_id);
            record_retry(
                state,
                InferenceRetryFact::DispatchLost {
                    reason: format!("broker admission rejected: {error}"),
                },
            )?;
            return Err(ShenkoError::other(format!(
                "broker admission rejected: {error}; outcome unknown, reconciled"
            )));
        }
        let result = rx
            .await
            .map_err(|_| ShenkoError::other("model outcome unknown; reconcile original request"))?;
        if result.request_id != request_id {
            return Err(ShenkoError::conflict(
                "model result request identity mismatch",
            ));
        }
        admit_model_result(state, &input, &result, claimed_send)?;
        let (output, stop_reason) =
            record_model_evidence(&store, &mut step, &state.head, seq, request_id, &result)?;
        state.step = step.clone();
        (Some(output), stop_reason)
    };

    // Hook: PostModel — SPEC §37. Fires after inference completes, before S1 commit.
    if let Some(reg) = &hooks_ref {
        let dispatch = reg
            .dispatch(&HookPayload::AfterInference {
                request_id: step_id,
                stop_reason: stop_reason.clone(),
                output_tokens: model_output.as_ref().map(|o| o.len() as u64).unwrap_or(0),
            })
            .await;
        debug!(seq, hooks=%dispatch.results.len(), graph=%reg.graph_digest(), "PostModel hooks ran");
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
            &state.channels,
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
        // An identity edit that does not fit the profile bounds is REFUSED,
        // never truncated: the subject is told and can correct it. The
        // refusal travels the same sourced correction path as any other
        // invalid control.
        for control in &output.controls {
            if let OutputAction::Identity { change } = &control.action {
                state.identity_service.check_change(change).map_err(|rejection| {
                    ShenkoError::Protocol(
                        shenko_types::error::ProtocolError::new(
                            "CONTROL_PROTOCOL_INVALID",
                            shenko_types::error::ErrorScope::Request,
                            shenko_types::error::RetryStrategy::NewRevision,
                        )
                        .with_details(serde_json::json!({
                            "line": control.spans.first().map(|span| span.line),
                            "reason": rejection.to_string(),
                        })),
                    )
                })?;
            }
        }
        // Dispositions and obligation origins may only name events this step
        // actually selected.
        // The model naming an event it was never given is an invalid
        // control, not a broken runtime: it travels the same sourced
        // correction path as any other invalid control, so the subject is
        // told and can correct it instead of the whole loop stopping and
        // waiting for a reopen.
        for control in &output.controls {
            let invalid = match &control.action {
                OutputAction::Dispose { spec }
                    if !input.events.iter().any(|event| event.event_id == spec.event_id) =>
                        Some("disposition names an event that was not selected into this step"),
                OutputAction::Obligation { spec }
                    if spec.origin_event_id.is_some_and(|origin|
                        !input.events.iter().any(|event| event.event_id == origin)) =>
                        Some("obligation names an input event that was not selected into this step"),
                _ => None,
            };
            if let Some(reason) = invalid {
                return Err(ShenkoError::Protocol(
                    shenko_types::error::ProtocolError::new(
                        "CONTROL_PROTOCOL_INVALID",
                        shenko_types::error::ErrorScope::Request,
                        shenko_types::error::RetryStrategy::NewRevision,
                    )
                    .with_details(serde_json::json!({
                        "line": control.spans.first().map(|span| span.line),
                        "reason": reason,
                    })),
                ));
            }
        }
        // Accepting an unknown effect may only name a case this step was
        // actually shown, and may only cite evidence that case carries
        // (SPEC §41.4 X11, §51.1). Anything else is an invalid control, not a
        // runtime failure.
        for control in &output.controls {
            let OutputAction::Reconcile { spec } = &control.action else {
                continue;
            };
            let Some(entry) = input
                .reconciliation_docket
                .iter()
                .find(|entry| entry.case.case_id == spec.case_id)
            else {
                return Err(ShenkoError::Protocol(
                    shenko_types::error::ProtocolError::new(
                        "CONTROL_PROTOCOL_INVALID",
                        shenko_types::error::ErrorScope::Request,
                        shenko_types::error::RetryStrategy::NewRevision,
                    )
                    .with_details(serde_json::json!({
                        "line": control.spans.first().map(|span| span.line),
                        "reason": "reconcile names a case that was not carried into this step",
                    })),
                ));
            };
            if spec
                .reason_refs
                .iter()
                .any(|reason| !entry.case.evidence_refs.contains(reason))
            {
                return Err(ShenkoError::Protocol(
                    shenko_types::error::ProtocolError::new(
                        "CONTROL_PROTOCOL_INVALID",
                        shenko_types::error::ErrorScope::Request,
                        shenko_types::error::RetryStrategy::NewRevision,
                    )
                    .with_details(serde_json::json!({
                        "line": control.spans.first().map(|span| span.line),
                        "reason": "reconcile cites evidence this case does not carry",
                    })),
                ));
            }
        }
        // A durable obligation may only answer an input this step actually
        // selected: an origin the subject was never shown cannot become a
        // frozen fact of its commitment. Naming any other event is a
        // correctable authored control, exactly like an unselected
        // disposition, so the loop returns a sourced replacement instead of
        // rejecting the whole step after its result is already recorded.
        for control in &output.controls {
            if let OutputAction::Obligation { spec } = &control.action {
                let Some(origin) = spec.origin_event_id else {
                    continue;
                };
                if !input.events.iter().any(|event| event.event_id == origin) {
                    return Err(ShenkoError::Protocol(
                        shenko_types::error::ProtocolError::new(
                            "CONTROL_PROTOCOL_INVALID",
                            shenko_types::error::ErrorScope::Request,
                            shenko_types::error::RetryStrategy::NewRevision,
                        )
                        .with_details(serde_json::json!({
                            "line": control.spans.first().map(|span| span.line),
                            "reason": "obligation names an input event that was not selected into this step",
                            "origin_event_id": origin,
                        })),
                    ));
                }
            }
        }
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
    // P1: validate terminal targets against S0 membership before constructing
    // the decision. An invalid target must route through the correction path,
    // not abort the loop after the step is already ResultRecorded.
    if output.controls.iter().any(|control| matches!(control.action, OutputAction::ObligationTerminal { .. })) {
        for control in &output.controls {
            if let OutputAction::ObligationTerminal { change } = &control.action {
                if !input.obligations.iter().any(|item| item.obligation_id == change.obligation_id) {
                    let error = shenko_types::error::ProtocolError::new(
                        "CONTROL_PROTOCOL_INVALID",
                        shenko_types::error::ErrorScope::Request,
                        shenko_types::error::RetryStrategy::NewRevision,
                    )
                    .with_details(serde_json::json!({
                        "reason": "obligation terminal target was not shown in this step's S0",
                        "obligation_id": change.obligation_id,
                    }));
                    replace_rejected_output(state, input, model_output.as_deref().unwrap_or(""), error)?;
                    return Ok(None);
                }
            }
        }
    }
    // ResultRecorded (including real CAS bytes) is already durable before hooks/S1.
    step.state = SubjectStepState::Committed;
    step.record_revision = RecordRevision(step.record_revision.0 + 1);

    // Hook: PreEffect — SPEC §37. Fires before S1 commit is written to the ledger.
    if let Some(reg) = &hooks_ref {
        let dispatch = reg
            .dispatch(&HookPayload::BeforeOperation {
                operation_id: step_id,
                operation_name: format!("commit_step/{seq}"),
            })
            .await;
        debug!(seq, hooks=%dispatch.results.len(), graph=%reg.graph_digest(), "PreEffect hooks ran");
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
    let mut disposition_specs = vec![];
    let mut obligation_intents: Vec<PendingObligation> = vec![];
    let mut dialogue_intents: Vec<DialogueIntent> = vec![];
    let mut unknown_acknowledgements: Vec<
        shenko_types::evidence::ReconciliationAcknowledgement,
    > = vec![];
    let mut attention_policy: Option<AttentionPolicy> = None;
    let mut steer_specs = vec![];
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
            OutputAction::ObligationTerminal { change } => {
                policy_intents.push(PolicyIntent {
                    intent_id: control.action_id,
                    kind: PolicyKind::ObligationTerminal { change: change.clone() },
                    description: "subject obligation terminal change".into(),
                    target_revision: None,
                });
            }
            OutputAction::Dispose { spec } => {
                disposition_specs.push((control.action_id, spec.clone()));
            }
            OutputAction::Obligation { spec } => {
                obligation_intents.push(PendingObligation {
                    obligation_id: control.action_id,
                    description: spec.description.clone(),
                    depends_on_task_id: spec.depends_on_task_id,
                    resume_condition: spec.resume_condition.clone().unwrap_or_default(),
                    origin_event_id: spec.origin_event_id,
                    due_at: spec.due_at,
                });
            }
            OutputAction::Dialogue { spec } => {
                dialogue_intents.push(DialogueIntent {
                    intent_id: control.action_id,
                    target_task_id: spec.target_task_id,
                    message: spec.message.clone(),
                    in_reply_to: spec.in_reply_to,
                    direction: spec.direction,
                });
            }
            OutputAction::Dnd { spec } => {
                attention_policy = Some(AttentionPolicy {
                    intent_id: control.action_id,
                    state: spec.state,
                    sources: spec.sources.clone(),
                    reason: spec.reason.clone(),
                    review_after: spec.review_after,
                    created_step_id: step_id,
                });
            }
            OutputAction::Steer { spec } => {
                steer_specs.push((control.action_id, spec.clone()));
            }
            OutputAction::Reconcile { spec } => {
                unknown_acknowledgements.push(
                    shenko_types::evidence::ReconciliationAcknowledgement {
                        case_id: spec.case_id,
                        acknowledgement_id: control.action_id,
                        reason_refs: spec.reason_refs.clone(),
                        remaining_impact: spec.remaining_impact.clone(),
                    },
                );
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

    // Every selected event is disposed explicitly. An event the model did
    // not mention is acknowledged, never silently dropped: the disposition
    // index has to show what the subject did with each input.
    let mut authored: std::collections::HashMap<EntityId, EventDisposition> =
        std::collections::HashMap::new();
    for (_action_id, spec) in &disposition_specs {
        authored.insert(
            spec.event_id,
            EventDisposition {
                event_id: spec.event_id,
                disposition: spec.disposition,
                reason: spec.reason.clone(),
                priority: spec.priority,
                review_after: spec.review_after,
            },
        );
    }
    let event_dispositions: Vec<EventDisposition> = events
        .iter()
        .map(|ev| {
            authored
                .remove(&ev.event_id)
                .unwrap_or(EventDisposition {
                    event_id: ev.event_id.clone(),
                    disposition: Disposition::Acknowledged,
                    reason: None,
                    priority: None,
                    review_after: None,
                })
        })
        .collect();

    // The live steers this result actually adopted. Each one is sealed on
    // its own, bound to the admitted submission and to the event that
    // carried it; the store re-checks that binding at S1.
    let mut applied_steering_refs = vec![];
    for (_action_id, spec) in &steer_specs {
        let event = events
            .iter()
            .find(|event| {
                event
                    .payload
                    .get("submission_id")
                    .and_then(|value| value.as_str())
                    == Some(spec.submission_id.as_str())
            })
            .ok_or_else(|| {
                ShenkoError::conflict(
                    "steering names a submission that was not selected into this step",
                )
            })?;
        let mode = event
            .payload
            .get("mode")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_owned();
        let record = shenko_types::subject::AppliedSteeringRecord {
            schema_version: 1,
            steer_submission_id: spec.submission_id.clone(),
            steer_event_id: event.event_id,
            mode,
            decided_at_step_id: step_id,
        };
        applied_steering_refs.push(seal_step_object(
            &store,
            seq,
            &serde_json::to_vec(&record)?,
            "application/json",
            "applied_steering",
            "AppliedSteeringRecord",
            1,
        )?);
    }

    // 决定对象没有自引用；后继身份在 CAS 决定与数据库事务中保持一致。
    // schema_version 9: the sealed decision can also carry the subject's
    // explicit acceptance of an unknown external effect (SPEC §41.4 X11).
    // Schema 8 (versioned PublicationTarget only) is still admitted by the
    // Store, so already sealed decisions keep their meaning. See
    // shenko_store::channel::DECISION_SCHEMA_VERSION.
    let decision = StepDecision {
        schema_version: shenko_store::channel::DECISION_SCHEMA_VERSION,
        step_id,
        output: output.clone(),
        identity_intent,
        input_plan_ref: step
            .input_plan_ref
            .clone()
            .ok_or_else(|| ShenkoError::conflict("missing input plan"))?,
        model_result_ref: step.model_result_ref.clone(),
        applied_steering_refs,
        event_dispositions,
        delegated_task_intents: decision_delegation_intents.clone(),
        child_dialogue_intents: dialogue_intents,
        memory_intents: memory_candidates.iter().map(|(i, _)| i.clone()).collect(),
        policy_intents,
        publication_intents: publication_intents.clone(),
        pending_obligations: obligation_intents,
        unknown_acknowledgements,
        attention_policy,
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
        shenko_store::channel::DECISION_SCHEMA_VERSION,
    )?);

    match store.commit_step_and_advance_head(&step, &state.head, next_step_id) {
        Ok(receipt) => {
            // 只在提交明确成功后采用内存状态并发布。
            let (head, successor) = store.load_subject(state.agent_id)?;
            state.head = head;
            state.step = successor;
            // The reminder this step's request carried is answered here, and
            // only here: the committed step is the evidence that the assembled
            // context was dispatched. A refused, aborted or restarted assembly
            // never reaches this line, so the reminder stays available; the
            // durable answer keeps a successful retry inside the same window
            // from repeating it. The commit is already durable, so a journal
            // that cannot record the answer is reported, not rolled back.
            if let Some(reminder) = state.pending_reminder.take() {
                if let Err(error) = state.context_registry.acknowledge_rollover_reminder(&reminder) {
                    warn!(%error, window=%reminder.window, "committed request could not record its notebook reminder");
                }
            }
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
                    .dispatch(&HookPayload::AfterOperation {
                        operation_id: step_id,
                        outcome: "committed".to_string(),
                        effect_status: format!("commit/{seq}"),
                    })
                    .await;
                debug!(seq, hooks=%dispatch.results.len(), graph=%reg.graph_digest(), "PostEffect hooks ran");
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
    // The correction is a NEW request identity: the rejected attempt's
    // identity is spent and is never re-used.
    input.request_id = EntityId::new();
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
        12,
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
    if let Some(registry) = &state.contribution_registry {
        let seal = state.step.input_plan_ref.as_ref().expect("correction S0 committed");
        if let Some(usage) = contribution_usage(&input, seal)? {
            registry.record_usage(usage).map_err(|error| ShenkoError::conflict(error.to_string()))?;
        }
    }
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

/// Live delivery and restart interpret the same original receipt identically.
/// Only provider-terminal success may become model output; a rejection without
/// text is retry/refusal evidence, never an empty successful answer.
fn admit_model_result(
    state: &mut SubjectLoopState,
    input: &FrozenStepInput,
    result: &shenko_broker::request::InferenceResult,
    send_id: Option<EntityId>,
) -> Result<(), ShenkoError> {
    use shenko_broker::request::{InferenceStatus, TerminationKind};
    let binding = input.provider_binding.as_ref()
        .ok_or_else(|| ShenkoError::conflict("model result has no sealed binding"))?;
    if !result.effective.matches_seal(binding, &input.payload) {
        return Err(ShenkoError::Protocol(shenko_types::error::ProtocolError::new(
            "MODEL_PROVIDER_BINDING_MISMATCH",
            shenko_types::error::ErrorScope::Request,
            shenko_types::error::RetryStrategy::NewRevision,
        )));
    }
    // Restore observations against the receipt's own selected send, not a
    // later quota admission for the same sealed step.
    if let Some(send_id) = send_id {
        observe_quota_response(&state.store, binding, result, result.request_id, send_id);
    }
    match result.status {
        InferenceStatus::QueuedCapacityWait => {
            record_retry(state, InferenceRetryFact::NeverDispatched)?;
            return Err(ShenkoError::other("broker capacity rejection; never_dispatched persisted"));
        }
        InferenceStatus::Rejected { http_status: 429 } => {
            record_retry(state, InferenceRetryFact::ProviderRejected { http_status: 429 })?;
            return Err(ShenkoError::other("provider rejection persisted; same frozen request may resume"));
        }
        InferenceStatus::Rejected { http_status } => {
            return Err(ShenkoError::Protocol(shenko_types::error::ProtocolError::new(
                "PROVIDER_REJECTED", shenko_types::error::ErrorScope::Request,
                shenko_types::error::RetryStrategy::NewRevision,
            ).with_details(serde_json::json!({"http_status":http_status}))));
        }
        InferenceStatus::InvalidRequest { code } => {
            return Err(ShenkoError::Protocol(shenko_types::error::ProtocolError::new(
                code.as_str(), shenko_types::error::ErrorScope::Request,
                shenko_types::error::RetryStrategy::NewRevision,
            )));
        }
        InferenceStatus::Unavailable => {
            return Err(ShenkoError::other("MODEL_PROVIDER_UNAVAILABLE: no provider was dispatched"));
        }
        InferenceStatus::Success if result.termination.kind == TerminationKind::ProviderTerminal => {}
        InferenceStatus::Success => {
            return Err(ShenkoError::conflict("successful model result lacks provider terminal evidence"));
        }
        _ => {
            return Err(ShenkoError::other("model outcome not conclusive; preserve original awaiting_model for reconciliation"));
        }
    }
    if let Some(send_id) = send_id {
        settle_quota_terminal(&state.store, result, result.request_id, send_id);
    }
    Ok(())
}

/// One projection for both live admission and restart. The existing frozen
/// request, not a second copy of the preparer's plan, owns these identities.
fn contribution_usage(
    input: &FrozenStepInput,
    seal: &ObjectRef,
) -> Result<Option<shenko_context_policy::ContributionUsage>, ShenkoError> {
    // Records sealed before the SPEC §51.9 cutover spell the sourced-content
    // kind `fact`. The sealed bytes stay exactly as they were; this reader
    // migrates its own parsed copy through the one named migration.
    let read = |value: &serde_json::Value| {
        let mut value = value.clone();
        shenko_types::plugin_runtime::migrate_legacy_contribution_kinds(&mut value);
        value
    };
    let sent: Vec<shenko_context_policy::ContextContribution> = input.contributions.iter()
        .map(|value| serde::Deserialize::deserialize(read(value)))
        .collect::<Result<_, _>>()?;
    let omitted: Vec<shenko_context_policy::ContributionOmission> = input.contribution_omissions.iter()
        .map(|value| serde::Deserialize::deserialize(read(value)))
        .collect::<Result<_, _>>()?;
    let total = sent.iter().try_fold(0u64, |total, item| total.checked_add(item.tokens));
    if total != Some(input.contribution_tokens)
        || sent.iter().any(|item| item.scope.scope_id != input.context.source.scope_id || item.scope.task_id.is_some())
    {
        return Err(ShenkoError::conflict("sealed contribution scope/token evidence mismatch"));
    }
    if sent.is_empty() && omitted.is_empty() { return Ok(None); }
    Ok(Some(shenko_context_policy::ContributionUsage {
        request_id: input.request_id,
        request_revision: input.step_no,
        input_revision: input.context.source.scope_id.to_string(),
        seal_digest: seal.digest.clone(),
        sent, omitted, total_tokens: input.contribution_tokens,
    }))
}

/// A rejected S0 owns no consumption. Once Store accepts the step, adopting
/// its in-memory state precedes projection so a projection error cannot make
/// the caller treat a durable preparation as a fresh Planned step.
fn commit_s0(
    state: &mut SubjectLoopState,
    step: SubjectStep,
    input: &FrozenStepInput,
) -> Result<(), ShenkoError> {
    let seal = step.input_plan_ref.as_ref()
        .ok_or_else(|| ShenkoError::conflict("S0 contribution projection requires a sealed input"))?;
    let usage = contribution_usage(input, seal)?;
    state.store.upsert_step(&step, &state.head, state.step.record_revision)?;
    state.step = step;
    if let (Some(registry), Some(usage)) = (&state.contribution_registry, usage) {
        registry.record_usage(usage).map_err(|error| ShenkoError::conflict(error.to_string()))?;
    }
    Ok(())
}

/// Seal the exact hook graph this step dispatches under. The binding is the
/// runtime's own sealed form, so the sealed plan and the graph that actually
/// ran can be compared. An absent hook runtime is a typed refusal: an empty
/// graph would pretend that hooks ran.
fn seal_hook_graph(
    store: &shenko_store::store::Store,
    seq: u64,
    hooks: Option<&Arc<HookRegistry>>,
) -> Result<ObjectRef, ShenkoError> {
    let hooks = hooks.ok_or_else(|| {
        ShenkoError::other(
            "HOOK_RUNTIME_UNAVAILABLE: the subject cannot seal a step without a hook graph; no dispatch is admitted",
        )
    })?;
    let binding = hooks.binding();
    seal_step_object(
        store,
        seq,
        &serde_json::to_vec(&binding)?,
        "application/json",
        "hook_graphs",
        "HookGraphBinding",
        1,
    )
}

/// Seal the terminal model evidence of this step and move it to
/// `ResultRecorded`. The sealed text, the sealed outcome and the step
/// transition commit before any S1 decision exists, so a crash here leaves a
/// resumable step instead of a lost result.
fn record_model_evidence(
    store: &shenko_store::store::Store,
    step: &mut SubjectStep,
    head: &SubjectBranchHead,
    seq: u64,
    request_id: EntityId,
    result: &shenko_broker::request::InferenceResult,
) -> Result<(String, String), ShenkoError> {
    let response = result
        .response
        .as_ref()
        .ok_or_else(|| ShenkoError::other("successful model result missing response"))?;
    let output = response.content.clone();
    step.model_result_ref = Some(seal_step_object(
        store,
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
        raw_response: response.raw_response.clone(),
        usage: response
            .usage
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?,
    };
    step.model_outcome_ref = Some(seal_step_object(
        store,
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
    store.upsert_step(step, head, expected)?;
    Ok((output, response.stop_reason.clone()))
}

/// What a resumed `AwaitingModel` step found in its own durable receipts.
enum AwaitingRecovery {
    /// The terminal evidence of this step's own request already exists: the
    /// result is adopted. No new send happens.
    Adopted { output: String, stop_reason: String },
    /// A positive retry fact was committed. The next invocation uses the
    /// legal RetryPending transition and a new request identity; unknown
    /// dispatched debt is never released by that transition.
    RetryRecorded,
}

/// Consult the durable receipts of this step's own attempts. The terminal
/// evidence decides; nothing here starts a second send.
fn recover_awaiting_model(
    state: &mut SubjectLoopState,
    step: &mut SubjectStep,
    input: &FrozenStepInput,
) -> Result<AwaitingRecovery, ShenkoError> {
    let store = Arc::clone(&state.store);
    let attempts = store.subject_request_attempts_for_step(state.agent_id, step.step_id)?;
    let request_id = step.active_request_id
        .ok_or_else(|| ShenkoError::conflict("awaiting step has no active request"))?;
    let attempt = attempts.iter().find(|attempt| attempt.request_id == request_id)
        .ok_or_else(|| ShenkoError::conflict("awaiting request has no durable attempt"))?;
    // Older rejected attempts must not override a later active request. Read
    // the exact original receipt of the currently awaited identity only.
    if attempt.state == SubjectRequestState::Terminal {
        let Some(result) = crate::subject_receipt::read_terminal_result(&store, attempt)
            .map_err(|error| ShenkoError::storage(format!("{error:#}")))?
        else {
            return Err(ShenkoError::conflict("terminal request has no sealed result"));
        };
        if result.termination.kind != shenko_broker::request::TerminationKind::Unknown {
        admit_model_result(state, input, &result, attempt.send_id)?;
        let (output, stop_reason) = record_model_evidence(
            &store,
            step,
            &state.head,
            step.step_no,
            request_id,
            &result,
        )?;
        state.step = step.clone();
        return Ok(AwaitingRecovery::Adopted { output, stop_reason });
        }
    }
    // Only an Armed row proves the dispatch boundary never passed. Terminal
    // records carrying Unknown remain uncertain, just like Dispatched rows.
    if attempt.state != SubjectRequestState::Armed {
        // The outcome is unknown. The spent send stays armed and unsettled,
        // and the next attempt gets a NEW request identity.
        record_retry(
            state,
            InferenceRetryFact::DispatchLost {
                reason: "the original request left the process without terminal evidence".into(),
            },
        )?;
        return Ok(AwaitingRecovery::RetryRecorded);
    }
    // The request provably never dispatched: the send boundary never
    // passed. Persist that fact through the existing legal retry transition;
    // do not attempt AwaitingModel -> AwaitingModel or reuse the old identity.
    record_retry(state, InferenceRetryFact::NeverDispatched)?;
    Ok(AwaitingRecovery::RetryRecorded)
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod recovery_tests;
