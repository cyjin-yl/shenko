//! Subject outbox — the outbound half of the subject timeline. docs/specs/subject-runtime.md#spec-5-1, docs/specs/web-interface.md#spec-21-12
//!
//! The subject loop publishes each completed model output here so transports
//! (the Web UI, other channels) can render it without reaching into loop state.
use serde::{Deserialize, Serialize};

/// The text part identity shared by the committed projection and the
/// streamed surface: `message_id` is the step id, `part_id` names the part
/// inside it. One identity means a client that watched the stream absorbs
/// the committed message instead of appending the same text twice
/// (docs/specs/web-interface.md#spec-21-8, docs/specs/web-interface.md#spec-21-9, docs/specs/web-interface.md#spec-57-5).
pub const TEXT_PART_ID: &str = "text/0";

/// One outbound subject message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxMessage {
    /// What kind of output this is, e.g. `"thought"` or `"reply"`.
    pub kind: String,
    /// The subject step number that produced it.
    pub step: u64,
    pub step_id: shenko_types::ids::EntityId,
    pub commit_ref: shenko_types::commit::CommitRef,
    /// Receiver-local commit sequence used only for durable replay. Together
    /// with `step_id` it pages steps imported in one commit without confusing
    /// that position with the immutable source `commit_ref` above.
    pub replay_seq: u64,
    /// The model output text.
    pub text: String,
    /// Already validated and committed publication actions; never reparse text.
    pub publications: Vec<shenko_types::subject::PublicationIntent>,
    /// Identity revision in effect after this step (docs/specs/subject-runtime.md#spec-6), so transports
    /// can refresh the displayed name/soul when it changes.
    pub identity_revision: Option<u64>,
}

/// Replayable committed-only projection. Stable IDs let transports de-duplicate
/// a repeated snapshot independently of effect dispatch.
pub fn committed_after(
    store: &shenko_store::Store,
    subject_id: shenko_types::ids::EntityId,
    seq: u64,
    after_step_id: Option<shenko_types::ids::EntityId>,
    limit: usize,
) -> anyhow::Result<Vec<OutboxMessage>> {
    store
        .committed_decisions_after(subject_id, seq, after_step_id, limit)?
        .into_iter()
        .map(|(replay_seq, step, decision)| {
            let identity_revision = store
                .identity_receipt(step.step_id)?
                .and_then(|r| r.outcome.revision());
            Ok(OutboxMessage {
                kind: "thought".into(),
                step: step.step_no,
                step_id: step.step_id,
                commit_ref: step
                    .commit_ref
                    .ok_or_else(|| anyhow::anyhow!("missing committed ref"))?,
                replay_seq,
                text: decision.output.text,
                publications: decision.publication_intents,
                identity_revision,
            })
        })
        .collect()
}
