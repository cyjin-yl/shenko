//! Subject outbox — the outbound half of the subject timeline. SPEC §5.1, §21.12
//!
//! The subject loop publishes each completed model output here so transports
//! (the Web UI, other channels) can render it without reaching into loop state.
use serde::{Deserialize, Serialize};

/// One outbound subject message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxMessage {
    /// What kind of output this is, e.g. `"thought"` or `"reply"`.
    pub kind: String,
    /// The subject step number that produced it.
    pub step: u64,
    /// The model output text.
    pub content: String,
}
