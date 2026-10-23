//! The continuous main subject loop runner — SPEC §5.1
//! Never terminates on empty inbox. Continues with reflection if no events.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::info;
use anyhow::Result;

use shenko_types::ids::EntityId;
use shenko_types::subject::SemanticWait;
use crate::inbox::{Inbox, InboxEvent};
use crate::dispatch::ChildDispatcher;

/// State for the running subject loop.
pub struct SubjectLoopState {
    pub agent_id: EntityId,
    pub step_seq: Arc<AtomicU64>,
    pub inbox: Arc<Inbox>,
    pub dispatcher: Arc<ChildDispatcher>,
    pub active_wait: Option<SemanticWait>,
}

impl SubjectLoopState {
    pub fn new(agent_id: EntityId, inbox: Arc<Inbox>, dispatcher: Arc<ChildDispatcher>) -> Self {
        Self {
            agent_id,
            step_seq: Arc::new(AtomicU64::new(0)),
            inbox,
            dispatcher,
            active_wait: None,
        }
    }

    pub fn next_seq(&self) -> u64 {
        self.step_seq.fetch_add(1, Ordering::SeqCst)
    }
}

/// Main loop — runs indefinitely. Yields inference slot after each step.
/// SPEC §5.1: "没有外部消息也继续下一步"
pub async fn run_subject_loop(
    mut state: SubjectLoopState,
    cancel: tokio_util::sync::CancellationToken,
) {
    info!(agent_id = %state.agent_id, "subject loop started");

    loop {
        if cancel.is_cancelled() {
            info!("subject loop: shutdown signal received");
            break;
        }

        let seq = state.next_seq();

        // Collect available events (non-blocking, short timeout)
        let events = state.inbox.recv_batch(16, 50).await;

        // Check semantic wait
        if let Some(ref wait) = state.active_wait {
            if wait.is_expired() {
                info!(seq, "semantic wait expired, resuming normal thought");
                state.active_wait = None;
            } else {
                // Still waiting — poll with context (same model, prior + new context)
                // For now: check if any events satisfy wait condition
                if !events.is_empty() {
                    // New events may satisfy wait — clear and continue
                    state.active_wait = None;
                }
            }
        }

        // Execute one subject step
        execute_step(seq, &events, &state).await;

        // Yield inference capacity to children (tokio cooperative yield)
        tokio::task::yield_now().await;
    }

    info!("subject loop terminated");
}

async fn execute_step(seq: u64, events: &[InboxEvent], state: &SubjectLoopState) {
    // Stub: log step execution.
    // Full implementation: call broker → model → parse decision → commit S1.
    if seq % 100 == 0 {
        info!(
            agent_id = %state.agent_id,
            seq,
            events_count = events.len(),
            "subject step"
        );
    }
}
