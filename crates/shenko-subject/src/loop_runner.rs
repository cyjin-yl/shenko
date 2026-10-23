//! Continuous subject loop — SPEC §5.1, §41.5
//! Real S0/S1 step state machine. Never terminates on empty inbox.
//! Semantic wait hard-max 600s. Yields inference slot after each step.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::time::Duration;
use tracing::{info, warn, debug};
use shenko_types::ids::{EntityId, BranchId, ControllerEpoch, RecordRevision, RuntimeGeneration};
use shenko_types::subject::{SubjectStep, SubjectStepState, SemanticWait};
use shenko_types::commit::CommitRef;
use shenko_types::object::{ObjectRef, SchemaRef, Digest};
use crate::inbox::{Inbox, InboxEvent};
use crate::dispatch::ChildDispatcher;

pub struct SubjectLoopState {
    pub agent_id: EntityId,
    pub branch_id: BranchId,
    pub step_seq: Arc<AtomicU64>,
    pub inbox: Arc<Inbox>,
    pub dispatcher: Arc<ChildDispatcher>,
    pub active_wait: Option<SemanticWait>,
    pub parent_commit: Option<CommitRef>,
    pub store: Option<Arc<shenko_store::store::Store>>,
    pub broker: Option<shenko_broker::broker::BrokerHandle>,
    pub model: String,
    pub max_tokens: u64,
    /// Last model output — feeds the next step's attention context.
    pub last_thought: Option<String>,
    /// Rolling goals the subject is pursuing.
    pub goals: Vec<String>,
}

impl SubjectLoopState {
    pub fn new(agent_id: EntityId, inbox: Arc<Inbox>, dispatcher: Arc<ChildDispatcher>) -> Self {
        Self {
            agent_id,
            branch_id: BranchId::new(),
            step_seq: Arc::new(AtomicU64::new(1)),
            inbox,
            dispatcher,
            active_wait: None,
            parent_commit: None,
            store: None,
            broker: None,
            model: "claude-opus-5-5".to_string(),
            max_tokens: 4096,
            last_thought: None,
            goals: vec![],
        }
    }

    pub fn with_store(mut self, store: Arc<shenko_store::store::Store>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn with_broker(mut self, broker: shenko_broker::broker::BrokerHandle) -> Self {
        self.broker = Some(broker);
        self
    }

    pub fn next_seq(&self) -> u64 { self.step_seq.fetch_add(1, Ordering::SeqCst) }
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

    let mut idle_streak = 0u64;

    loop {
        if cancel.is_cancelled() {
            info!("subject loop: shutdown");
            break;
        }

        let seq = state.next_seq();

        // Collect available events — short timeout to stay responsive
        let events = state.inbox.recv_batch(32, 80).await;
        let has_events = !events.is_empty();

        if has_events { idle_streak = 0; }
        else { idle_streak += 1; }

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
        if let Some(new_commit) = execute_step(seq, &events, &mut state, in_semantic_wait).await {
            state.parent_commit = Some(new_commit);
        }

        // Yield inference slot to children — cooperative multitasking. SPEC §5.1
        tokio::task::yield_now().await;

        // Brief back-off when truly idle (no events for many consecutive steps),
        // but never turn into a low-frequency timer tick. SPEC §5.1: always continues.
        if idle_streak > 20 && !in_semantic_wait {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    info!("subject loop terminated");
}

/// System prompt for the continuous subject. SPEC §5.1
fn build_system_prompt(state: &SubjectLoopState, in_semantic_wait: bool) -> String {
    let mut prompt = String::from(
        "你是 Shenko（深刻），一个持续运行的个人 agent。\n\
         你有持续的内心生活：关注、兴趣、承诺、疑问。\n\
         你在没有外部消息时也继续思考——空闲不是停止的理由。\n\
         思考是连续的，上一步的想法会成为这一步的起点。",
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
fn build_messages(state: &SubjectLoopState, events: &[InboxEvent]) -> Vec<shenko_broker::request::InferenceMessage> {
    let mut messages = vec![];

    for ev in events.iter().take(10) {
        // OneBot message events carry the text under raw_message
        let text = ev.payload.get("raw_message")
            .or_else(|| ev.payload.get("message"))
            .and_then(|v| v.as_str());
        if let Some(text) = text {
            let who = ev.payload.get("nickname")
                .and_then(|v| v.as_str()).unwrap_or(&ev.source);
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

/// Execute one subject step: S0 (Planned→Preparing) → AwaitingModel → S1 (Committed).
/// Returns the commit ref if S1 succeeded.
async fn execute_step(
    seq: u64,
    events: &[InboxEvent],
    state: &mut SubjectLoopState,
    in_semantic_wait: bool,
) -> Option<CommitRef> {
    let store = state.store.as_ref()?;

    let step_id = EntityId::new();
    let _next_step_id = EntityId::new();

    let parent_commit = state.parent_commit.unwrap_or_else(|| CommitRef {
        ledger_id: store.ledger.ledger_id(),
        seq: shenko_types::ids::LedgerSeq(0),
    });

    // S0: Planned → Preparing — seal input plan
    let mut step = SubjectStep {
        step_id,
        branch_id: state.branch_id.clone(),
        step_no: seq,
        expected_parent_commit: parent_commit,
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration::default(),
        state: SubjectStepState::Preparing,
        input_plan_ref: None,
        context_revision: Some(seq),
        active_request_id: None,
        active_attempt_id: None,
        model_result_ref: None,
        decision_ref: None,
        commit_ref: None,
        retry_reason_ref: None,
        record_revision: RecordRevision(0),
        created_by_step_id: None,
    };

    if let Err(e) = store.upsert_step(&step) {
        warn!(seq, err=%e, "S0 Preparing commit failed");
        return None;
    }

    // AwaitingModel: real inference via broker
    let model_output = match &state.broker {
        Some(broker) => {
            step.state = SubjectStepState::AwaitingModel;
            let request_id = EntityId::new();
            step.active_request_id = Some(request_id);
            let _ = store.upsert_step(&step);

            let (tx, rx) = tokio::sync::oneshot::channel();
            let req = shenko_broker::request::InferenceRequest {
                request_id,
                agent_id: state.agent_id,
                conversation_id: state.agent_id,
                model: state.model.clone(),
                priority: shenko_broker::request::InferencePriority::Subject,
                epoch: ControllerEpoch(1),
                payload: shenko_broker::request::InferencePayload {
                    system_prompt: build_system_prompt(state, in_semantic_wait),
                    messages: build_messages(state, events),
                    max_tokens: Some(state.max_tokens),
                    temperature: Some(0.7),
                    tools: vec![],
                },
                result_tx: tx,
            };

            match broker.submit(req) {
                Ok(()) => match rx.await {
                    Ok(result) => {
                        use shenko_broker::request::InferenceStatus;
                        match result.status {
                            InferenceStatus::Success => result.response.map(|r| r.content),
                            InferenceStatus::Failed => {
                                warn!(seq, "model inference failed");
                                None
                            }
                            InferenceStatus::QueuedCapacityWait => {
                                debug!(seq, "inference queued — capacity wait");
                                None
                            }
                            InferenceStatus::Cancelled => None,
                        }
                    }
                    Err(_) => {
                        warn!(seq, "inference channel closed");
                        None
                    }
                },
                Err(e) => {
                    warn!(seq, err=%e, "broker submit failed");
                    None
                }
            }
        }
        None => {
            debug!(seq, "no broker configured — step proceeds without model");
            None
        }
    };

    // Carry thought forward
    if let Some(output) = &model_output {
        state.last_thought = Some(output.clone());
    }

    // S1: Committed — atomic commit of decision + successor. SPEC §41.5
    step.state = SubjectStepState::Committed;
    step.record_revision = RecordRevision(1);
    if let Some(output) = &model_output {
        let digest = Digest::blake3(output.as_bytes());
        step.model_result_ref = Some(ObjectRef {
            digest,
            bytes: output.len() as u64,
            media_type: "text/plain".into(),
            store_namespace: "model_results".into(),
            locator: String::new(),
            codec: SchemaRef { name: "text".into(), major: 1, digest: Digest::blake3(b"text") },
        });
    }

    match store.upsert_step(&step) {
        Ok(receipt) => {
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
            Some(receipt.commit_ref)
        }
        Err(e) => {
            warn!(seq, err=%e, "S1 commit failed");
            None
        }
    }
}
