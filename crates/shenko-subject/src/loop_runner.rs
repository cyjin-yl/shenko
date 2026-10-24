//! Continuous subject loop — SPEC §5.1, §41.5
//! Real S0/S1 step state machine. Never terminates on empty inbox.
//! Semantic wait hard-max 600s. Yields inference slot after each step.
use crate::dispatch::ChildDispatcher;
use crate::inbox::{Inbox, InboxEvent};
use crate::outbox::OutboxMessage;
use shenko_kernel::hooks::{HookPayload, HookRegistry};
use shenko_types::commit::CommitRef;
use shenko_types::error::ShenkoError;
use shenko_types::ids::{EntityId, RecordRevision};
use shenko_types::object::ObjectRef;
use shenko_types::subject::{
    DelegationIntent, Disposition, EventDisposition, InboxCursor, MemoryIntent, MemoryIntentKind,
    SemanticWait, StepDecision, SubjectBranchHead, SubjectStep, SubjectStepState,
};
use std::sync::Arc;
use tracing::{debug, info, warn};

pub struct SubjectLoopState {
    pub agent_id: EntityId,
    pub head: SubjectBranchHead,
    pub step: SubjectStep,
    pub inbox: Arc<Inbox>,
    pub dispatcher: Arc<ChildDispatcher>,
    pub active_wait: Option<SemanticWait>,
    pub store: Arc<shenko_store::store::Store>,
    pub broker: Option<shenko_broker::broker::BrokerHandle>,
    pub hooks: Option<Arc<HookRegistry>>,
    pub model: String,
    pub max_tokens: u64,
    /// Last model output — feeds the next step's attention context.
    pub last_thought: Option<String>,
    /// Rolling goals the subject is pursuing.
    pub goals: Vec<String>,
    /// Monotonically increasing count of inbox events consumed across steps — used for InboxCursor.
    pub last_inbox_cursor: u64,
    /// Outbound path for completed model output — SPEC §5.1, §21.12.
    pub outbox_tx: Option<tokio::sync::broadcast::Sender<OutboxMessage>>,
}

impl SubjectLoopState {
    /// 从真实 head/唯一后继恢复；未知模型状态不允许重新发起推理。
    pub fn new(
        agent_id: EntityId,
        inbox: Arc<Inbox>,
        dispatcher: Arc<ChildDispatcher>,
        store: Arc<shenko_store::store::Store>,
    ) -> Result<Self, ShenkoError> {
        let (head, step) = store.load_subject(agent_id)?;
        if step.state != SubjectStepState::Planned
            || step.active_request_id.is_some()
            || step.active_attempt_id.is_some()
        {
            return Err(ShenkoError::conflict(format!(
                "recovery_blocked: step {} is {:?}; reconcile original request/attempt and frozen input before inference",
                step.step_id, step.state,
            )));
        }
        let last_thought = if let Some(id) = head.committed_step_id {
            let previous = store
                .get_step(&head.branch_id, id)?
                .ok_or_else(|| ShenkoError::not_found("committed subject step"))?;
            if previous.state != SubjectStepState::Committed
                || previous.commit_ref != Some(head.committed_at)
            {
                return Err(ShenkoError::conflict("committed head/step mismatch"));
            }
            previous
                .model_result_ref
                .map(|object| {
                    let bytes = store.cas.read_object(&object.digest)?;
                    if bytes.len() as u64 != object.bytes
                        || shenko_types::object::Digest::blake3(&bytes) != object.digest
                    {
                        return Err(ShenkoError::ObjectClosureIncomplete { missing_count: 1 });
                    }
                    String::from_utf8(bytes).map_err(|e| ShenkoError::storage(e.to_string()))
                })
                .transpose()?
        } else {
            None
        };
        Ok(Self {
            agent_id,
            head,
            step,
            inbox,
            dispatcher,
            active_wait: None,
            store,
            broker: None,
            hooks: None,
            model: "claude-opus-5-5".to_string(),
            max_tokens: 4096,
            last_thought,
            goals: vec![],
            last_inbox_cursor: 0,
            outbox_tx: None,
        })
    }

    pub fn with_broker(mut self, broker: shenko_broker::broker::BrokerHandle) -> Self {
        self.broker = Some(broker);
        self
    }

    pub fn with_hooks(mut self, registry: Arc<HookRegistry>) -> Self {
        self.hooks = Some(registry);
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
) {
    info!(agent_id=%state.agent_id, "subject loop started");

    loop {
        if cancel.is_cancelled() {
            info!("subject loop: shutdown");
            break;
        }

        let seq = state.step.step_no;

        // Collect available events — short timeout to stay responsive
        let events = state.inbox.recv_batch(32, 80).await;
        let has_events = !events.is_empty();

        // Check semantic wait — SPEC §5.1/U55, §41.5
        let in_semantic_wait = match state.active_wait.as_ref() {
            Some(wait) if wait.is_expired() => {
                info!(seq, "semantic wait expired — resuming normal thought");
                state.active_wait = None;
                false
            }
            Some(_) if has_events => {
                state.active_wait = None;
                false
            }
            Some(_) => true,
            None => false,
        };

        // Execute one subject step (S0 → AwaitingModel → Committed)
        if let Err(error) = execute_step(seq, &events, &mut state, in_semantic_wait).await {
            // 失败或结果未知后关闭本运行器；不能在旧内存状态中造一个新步骤。
            warn!(seq, err=%error, "主体恢复阻塞，停止新决定，等待重开和对账");
            break;
        }

        // Yield inference slot to children — cooperative multitasking. SPEC §5.1
        tokio::task::yield_now().await;
    }

    info!("subject loop terminated");
}

/// System prompt for the continuous subject. SPEC §5.1
fn build_system_prompt(state: &SubjectLoopState, in_semantic_wait: bool) -> String {
    let mut prompt = String::from(
        "你是 Shenko（深刻），一个持续运行的个人 agent。\n\
         你有持续的内心生活：关注、兴趣、承诺、疑问。\n\
         你在没有外部消息时也继续思考——空闲不是停止的理由。\n\
         思考是连续的，上一步的想法会成为这一步的起点。\n\
         回复 QQ 时输出 \"[reply:group:群号] 内容\" 或 \"[reply:private:QQ号] 内容\"。",
    );

    if !state.goals.is_empty() {
        prompt.push_str("\n\n当前关注的方向：\n");
        for g in &state.goals {
            prompt.push_str(&format!("- {g}\n"));
        }
    }

    if let Some(prev) = &state.last_thought {
        prompt.push_str("\n\n你上一步的想法（继续这条思路，不要重新开始）：\n");
        prompt.push_str(prev);
        prompt.push('\n');
    }

    if in_semantic_wait {
        prompt.push_str("\n\n当前处于语义等待：条件尚未满足。继续思考相关可能性，不要空转。\n");
    }

    prompt
}

/// Build the message list from recent events + carried thought.
///
/// Event text may arrive under different keys depending on the source:
/// OneBot/NapCat uses `raw_message`, other channels use `message`, and the
/// WebUI sends `content` (see `shenko-ui/src/ws.rs`). Non-text payloads and
/// text-less system events are skipped rather than crashing the step.
fn build_messages(
    _state: &SubjectLoopState,
    events: &[InboxEvent],
) -> Vec<shenko_broker::request::InferenceMessage> {
    let mut messages = vec![];

    for ev in events.iter().take(10) {
        let text = ["raw_message", "message", "content", "text"]
            .iter()
            .find_map(|key| ev.payload.get(*key).and_then(|v| v.as_str()))
            .map(str::trim)
            .filter(|text| !text.is_empty());
        if let Some(text) = text {
            let who = ev
                .payload
                .get("nickname")
                .and_then(|v| v.as_str())
                .unwrap_or(&ev.source);
            messages.push(shenko_broker::request::InferenceMessage {
                role: "user".to_string(),
                content: format!("[{who}] {text}"),
            });
        }
    }

    if messages.is_empty() {
        // Continuous thought — no external input. SPEC §5.1
        messages.push(shenko_broker::request::InferenceMessage {
            role: "user".to_string(),
            content: "继续。".to_string(),
        });
    }

    messages
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

/// Extract explicitly tagged memory content from model output. SPEC §12.2.
///
/// Recognises `记住`, `[remember]` and `[memory]`; honours at most one marker
/// per step and returns the rest of that marker's line, trimmed.
fn extract_memory_content(output: &str) -> Option<String> {
    const MARKERS: [&str; 3] = ["记住", "[remember]", "[memory]"];
    // ASCII-only lowercasing keeps byte offsets valid for the original text.
    let lower = output.to_ascii_lowercase();
    let mut content_start: Option<usize> = None;
    for marker in MARKERS {
        let haystack = if marker.is_ascii() {
            lower.as_str()
        } else {
            output
        };
        if let Some(pos) = haystack.find(marker) {
            let end = pos + marker.len();
            content_start = Some(content_start.map_or(end, |cur| cur.min(end)));
        }
    }
    let rest = &output[content_start?..];
    let content = rest.lines().next().unwrap_or("").trim();
    if content.is_empty() {
        None
    } else {
        Some(content.to_string())
    }
}

/// Execute one subject step: S0 (Planned→Preparing) → AwaitingModel → S1 (Committed).
/// Returns the commit ref if S1 succeeded.
async fn execute_step(
    seq: u64,
    events: &[InboxEvent],
    state: &mut SubjectLoopState,
    in_semantic_wait: bool,
) -> Result<CommitRef, ShenkoError> {
    let store = Arc::clone(&state.store);
    let hooks_ref = state.hooks.clone(); // Option<Arc<HookRegistry>> — cloned to avoid reborrow

    let step_id = state.step.step_id;
    let next_step_id = EntityId::new();
    let mut step = state.step.clone();
    if step.state != SubjectStepState::Planned || step.step_no != seq {
        return Err(ShenkoError::conflict(
            "step is not the registered planned successor",
        ));
    }

    // S0: seal input plan — SPEC §41.5
    // Build a content-addressed input plan from the step seq + event ids so each
    // step fixes its attention context version before the model is called.
    // The bytes are sealed into CAS so the ref is actually readable back.
    let mut plan_buf: Vec<u8> = Vec::new();
    plan_buf.extend_from_slice(seq.to_le_bytes().as_slice());
    for ev in events {
        let id_bytes = ev.event_id.as_uuid().as_bytes();
        plan_buf.extend_from_slice(id_bytes);
    }
    let input_plan_ref = seal_step_object(
        &store,
        seq,
        &plan_buf,
        "application/octet-stream",
        "input_plans",
        "input_plan",
        1,
    )?;

    // Advance the inbox cursor to the last event consumed by this step.
    let last_event_id = events.last().map(|ev| ev.event_id.clone());
    state.last_inbox_cursor = state.last_inbox_cursor.saturating_add(events.len() as u64);
    let _inbox_cursor = InboxCursor {
        through_event_id: last_event_id,
        consumed_count: state.last_inbox_cursor,
    };

    // S0 的抢占以持久 step revision 为准，同一步只有一个准备者能继续。
    step.state = SubjectStepState::Preparing;
    step.input_plan_ref = Some(input_plan_ref);
    step.context_revision = Some(seq);
    step.record_revision = RecordRevision(step.record_revision.0 + 1);
    store.upsert_step(&step, &state.head, state.step.record_revision)?;
    state.step = step.clone();

    // Hook: PreIngress — SPEC §37. Fires before messages are built and inference begins.
    if let Some(reg) = &hooks_ref {
        let results = reg
            .run_phase(&HookPayload::BeforeInference {
                request_id: step_id,
                agent_id: state.agent_id,
                model: state.model.clone(),
            })
            .await;
        debug!(seq, hooks=%results.len(), "PreIngress hooks ran");
    }

    // AwaitingModel: real inference via broker
    let model_output = match &state.broker {
        Some(broker) => {
            step.state = SubjectStepState::AwaitingModel;
            let request_id = EntityId::new();
            step.active_request_id = Some(request_id);
            let expected_revision = step.record_revision;
            step.record_revision = RecordRevision(expected_revision.0 + 1);
            store.upsert_step(&step, &state.head, expected_revision)?;
            state.step = step.clone();

            let (tx, rx) = tokio::sync::oneshot::channel();
            let req = shenko_broker::request::InferenceRequest {
                request_id,
                agent_id: state.agent_id,
                conversation_id: state.agent_id,
                model: state.model.clone(),
                priority: shenko_broker::request::InferencePriority::Subject,
                epoch: state.head.controller_epoch,
                payload: shenko_broker::request::InferencePayload {
                    system_prompt: build_system_prompt(state, in_semantic_wait),
                    messages: build_messages(state, events),
                    max_tokens: Some(state.max_tokens),
                    tools: vec![],
                },
                result_tx: tx,
            };

            broker.submit(req).map_err(|e| {
                ShenkoError::other(format!("broker admission failed; reconcile request: {e}"))
            })?;
            let result = rx.await.map_err(|_| {
                ShenkoError::other("model outcome unknown; reconcile original request")
            })?;
            if result.request_id != request_id
                || !matches!(
                    result.status,
                    shenko_broker::request::InferenceStatus::Success
                )
            {
                return Err(ShenkoError::other(
                    "model has no accepted complete result; keep awaiting_model for reconciliation",
                ));
            }
            Some(
                result
                    .response
                    .ok_or_else(|| ShenkoError::other("successful model result missing response"))?
                    .content,
            )
        }
        None => {
            return Err(ShenkoError::other(
                "no broker; step cannot commit a model decision",
            ));
        }
    };

    // Hook: PostModel — SPEC §37. Fires after inference completes, before S1 commit.
    if let Some(reg) = &hooks_ref {
        let results = reg
            .run_phase(&HookPayload::AfterInference {
                request_id: step_id,
                stop_reason: model_output
                    .as_ref()
                    .map(|_| "stop")
                    .unwrap_or("none")
                    .to_string(),
                output_tokens: model_output.as_ref().map(|o| o.len() as u64).unwrap_or(0),
            })
            .await;
        debug!(seq, hooks=%results.len(), "PostModel hooks ran");
    }

    // S1: Committed — atomic commit of step + advance branch head in one TX. SPEC §41.2.1/§44.6
    step.state = SubjectStepState::ResultRecorded;
    let expected_revision = step.record_revision;
    step.record_revision = RecordRevision(expected_revision.0 + 1);
    if let Some(output) = &model_output {
        // Seal the actual model bytes before the S1 commit so the ref points
        // at durable, readable content. SPEC §41.2.2.
        step.model_result_ref = Some(seal_step_object(
            &store,
            seq,
            output.as_bytes(),
            "text/plain",
            "model_results",
            "text",
            1,
        )?);
    }

    store.upsert_step(&step, &state.head, expected_revision)?;
    state.step = step.clone();
    step.state = SubjectStepState::Committed;
    step.record_revision = RecordRevision(step.record_revision.0 + 1);

    // Hook: PreEffect — SPEC §37. Fires before S1 commit is written to the ledger.
    if let Some(reg) = &hooks_ref {
        let results = reg
            .run_phase(&HookPayload::BeforeOperation {
                operation_id: step_id,
                operation_name: format!("commit_step/{seq}"),
            })
            .await;
        debug!(seq, hooks=%results.len(), "PreEffect hooks ran");
    }

    // S1 decision — SPEC §41.5: commit decision + event disposition +
    // delegation intent + unique successor before the ledger write.
    let output_text = model_output.as_deref().unwrap_or("");
    let lower_out = output_text.to_lowercase();
    let delegation_trigger = output_text.contains("委派")
        || output_text.contains("delegate")
        || lower_out.contains("[delegate]")
        || lower_out.contains("dispatch:");

    let mut decision_delegation_intents: Vec<DelegationIntent> = vec![];
    if delegation_trigger {
        decision_delegation_intents.push(DelegationIntent {
            intent_id: EntityId::new(),
            goal: output_text
                .lines()
                .next()
                .unwrap_or("delegated task")
                .to_string(),
            system_prompt: output_text.to_string(),
            model: None,
            capabilities: vec![],
            budget_hint: None,
            reply_grant: None,
        });
    }

    let event_dispositions: Vec<EventDisposition> = events
        .iter()
        .map(|ev| EventDisposition {
            event_id: ev.event_id.clone(),
            disposition: Disposition::Acknowledged,
        })
        .collect();

    // Memory intent — SPEC §12.2. At most one save intent per step, extracted
    // only from an explicit marker in the model output. The tagged content is
    // sealed into CAS and the intent is recorded in the S1 decision; the entry
    // itself is committed after the S1 commit succeeds.
    let memory_candidate: Option<(MemoryIntent, String)> = model_output
        .as_deref()
        .and_then(extract_memory_content)
        .map(|content| {
            let content_ref = seal_step_object(
                &store,
                seq,
                content.as_bytes(),
                "text/plain",
                "memory_intents",
                "text",
                1,
            )?;
            let intent = MemoryIntent {
                intent_id: EntityId::new(),
                kind: MemoryIntentKind::Save,
                scope: "personal".into(),
                content_ref: Some(content_ref),
            };
            Ok::<_, ShenkoError>((intent, content))
        })
        .transpose()?;
    let decision_memory_intents: Vec<MemoryIntent> = memory_candidate
        .iter()
        .map(|(intent, _)| intent.clone())
        .collect();

    // 决定对象没有自引用；后继身份在 CAS 决定与数据库事务中保持一致。
    let decision = StepDecision {
        step_id,
        input_plan_ref: step
            .input_plan_ref
            .clone()
            .ok_or_else(|| ShenkoError::conflict("missing input plan"))?,
        model_result_ref: step.model_result_ref.clone(),
        applied_steering_refs: vec![],
        event_dispositions,
        delegated_task_intents: decision_delegation_intents.clone(),
        child_dialogue_intents: vec![],
        memory_intents: decision_memory_intents,
        policy_intents: vec![],
        publication_intents: vec![],
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
        1,
    )?);

    match store.commit_step_and_advance_head(&step, &state.head, next_step_id) {
        Ok(receipt) => {
            // 只在提交明确成功后采用内存状态并发布。
            let (head, successor) = store.load_subject(state.agent_id)?;
            state.head = head;
            state.step = successor;
            state.last_thought = model_output.clone();
            // Outbound path — publish ONLY after the S1 commit succeeded.
            // SPEC §41.2.3 committed-only publication: never show a thought
            // that is not durably recorded, or a retried step could show a
            // different one.
            if let Some(output) = &model_output {
                if let Some(tx) = &state.outbox_tx {
                    let _ = tx.send(OutboxMessage {
                        kind: "thought".into(),
                        step: seq,
                        content: output.clone(),
                    });
                }
            }

            // Memory pipeline — SPEC §12.2: accept the intent then commit its
            // entry. Runs only after the S1 commit is durable and at most once
            // per step.
            if let Some((intent, content)) = &memory_candidate {
                let record = shenko_store::memory::MemoryIntentRecord::accepted(
                    intent.intent_id,
                    state.agent_id,
                    "note",
                    intent.scope.clone(),
                    content.clone(),
                    intent.content_ref.as_ref().map(|r| r.locator.clone()),
                    None,
                );
                match store.insert_memory_intent(&record) {
                    Ok(()) => {
                        let entry =
                            shenko_store::memory::MemoryEntry::new(state.agent_id, content.clone());
                        if let Err(e) = store.commit_memory_intent(&entry, intent.intent_id) {
                            warn!(seq, err=%e, "memory intent commit failed");
                            let _ = store.fail_memory_intent(intent.intent_id, &e.to_string());
                        }
                    }
                    Err(e) => warn!(seq, err=%e, "memory intent insert failed"),
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
                let results = reg
                    .run_phase(&HookPayload::AfterOperation {
                        operation_id: step_id,
                        outcome: "committed".to_string(),
                        effect_status: format!("commit/{seq}"),
                    })
                    .await;
                debug!(seq, hooks=%results.len(), "PostEffect hooks ran");
            }

            // Dispatch delegation intents sealed into the S1 decision. SPEC §5.2/§5.3.
            // Fire after commit so intent is only dispatched once durably committed.
            for intent in decision_delegation_intents {
                if let Err(e) = state.dispatcher.dispatch(intent).await {
                    warn!(seq, err=%e, "delegation dispatch failed");
                }
            }

            Ok(receipt.commit_ref)
        }
        Err(e) => {
            warn!(seq, err=%e, "S1 commit failed");
            Err(e)
        }
    }
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod recovery_tests;
