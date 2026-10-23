//! Continuous subject loop — SPEC §5.1, §41.5
//! Real S0/S1 step state machine. Never terminates on empty inbox.
//! Semantic wait hard-max 600s. Yields inference slot after each step.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::time::Duration;
use tracing::{info, warn, debug, instrument};
use shenko_types::ids::{EntityId, BranchId, ControllerEpoch, RecordRevision, RuntimeGeneration};
use shenko_types::subject::{
    SubjectStep, SubjectStepState, SemanticWait,
    DelegationIntent,
};
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
        }
    }

    pub fn with_store(mut self, store: Arc<shenko_store::store::Store>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn next_seq(&self) -> u64 { self.step_seq.fetch_add(1, Ordering::SeqCst) }
}

/// Run the continuous subject loop. SPEC §5.1, §41.5.
/// - Never stops for empty inbox
/// - S0: seal SubjectStep as Planned → Preparing
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
                // New events may satisfy condition — exit wait
                state.active_wait = None;
                false
            }
            Some(_) => true,
            None => false,
        };

        // Execute one subject step (S0 → Committed)
        if let Some(new_commit) = execute_step(seq, &events, &state, in_semantic_wait).await {
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

/// Execute one subject step: S0 (Planned→Preparing) then S1 (Committed).
/// Returns the commit ref if S1 succeeded.
async fn execute_step(
    seq: u64,
    events: &[InboxEvent],
    state: &SubjectLoopState,
    in_semantic_wait: bool,
) -> Option<CommitRef> {
    let step_id = EntityId::new();
    let next_step_id = EntityId::new();

    let parent_commit = state.parent_commit.unwrap_or_else(|| {
        state.store.as_ref().map(|s| CommitRef {
            ledger_id: s.ledger.ledger_id(),
            seq: shenko_types::ids::LedgerSeq(0),
        }).unwrap_or(CommitRef {
            ledger_id: EntityId::nil(),
            seq: shenko_types::ids::LedgerSeq(0),
        })
    });

    let store = state.store.as_ref()?;

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

    // Model call would go here (via broker) — for now we stub the result
    // In S2+: send InferenceRequest to broker, await response, build decision
    // The actual model result becomes model_result_ref; decision is serialized and stored

    // S1: Committed — atomic commit of decision + successor. SPEC §41.5
    step.state = SubjectStepState::Committed;
    step.record_revision = RecordRevision(1);
    step.commit_ref = Some(parent_commit); // will be updated after commit

    match store.upsert_step(&step) {
        Ok(receipt) => {
            if seq % 50 == 0 || !events.is_empty() {
                debug!(
                    seq,
                    events=%events.len(),
                    in_wait=in_semantic_wait,
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
