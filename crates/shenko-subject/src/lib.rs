//! Continuous subject loop — SPEC §5.1
//! "没有外部消息也继续下一步" — subject never stops for empty queue.
pub mod child;
pub mod dispatch;
pub mod inbox;
mod authored_controls;
mod context_projection;
pub mod loop_runner;
pub mod outbox;
pub mod model_child;
pub mod intervention;
mod quota_gate;
mod subject_receipt;
pub mod quota_runtime;
pub mod quota_strategies;
pub mod stream_journal;

pub use child::*;
pub use dispatch::*;
pub use inbox::*;
pub use loop_runner::*;
pub use outbox::*;
pub use model_child::ModelChildHandler;
pub use intervention::PmaEvaluateHandler;
pub use quota_strategies::{QuotaStrategyPlugin, QuotaStrategyService};
pub use stream_journal::SubjectStreamJournal;

pub mod output;

pub use authored_controls::public_text_prefix;
pub use context_projection::{IDLE_CONTINUATION_PROMPT, staged_message_text};

/// The stable system-prompt prefix every subject step ships: the subject
/// contract followed by the output protocol, in the order SPEC §5.1/§6
/// defines. Behavior evaluation binds to this exact text instead of a copy, so
/// a prompt change cannot silently pass a stale harness.
pub fn canonical_system_prompt(channels: &shenko_types::channel::ChannelRegistry) -> String {
    let mut prompt = String::from(loop_runner::SUBJECT_CONTRACT);
    prompt.push_str("\n\n");
    prompt.push_str(&output::output_contract(channels));
    prompt
}

/// The stable prefix with the subject's own role frame — SPEC §57.1.
///
/// The frame replaces only the role text; the output protocol stays the
/// kernel's, because it is the ABI the control-line parser and the channel
/// plugins depend on. A frame that wanted a different output protocol would
/// be a different kernel, not a different frame.
pub fn subject_prompt_with_frame(
    frame: &str,
    channels: &shenko_types::channel::ChannelRegistry,
) -> String {
    let mut prompt = String::from(frame);
    prompt.push_str("\n\n");
    prompt.push_str(&output::output_contract(channels));
    prompt
}
