use serde::{Deserialize, Serialize};

/// A message with a role and content, compatible with inference context messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceMessage {
    pub role: String,
    pub content: String,
}

/// Manifest describing a compact plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactManifest {
    pub plugin_id: &'static str,
    pub version: &'static str,
    pub max_tokens: usize,
}

/// Budget-driven compaction plugin.
///
/// Triggers when the context token count exceeds 85 % of the configured
/// maximum, then trims oldest user/assistant pairs to bring the context
/// back within budget.  System messages are always preserved.
#[derive(Debug, Clone)]
pub struct BudgetCompactPlugin {
    pub max_tokens: usize,
    pub current_tokens: usize,
}

impl BudgetCompactPlugin {
    pub fn new(max_tokens: usize) -> Self {
        Self { max_tokens, current_tokens: 0 }
    }

    /// Returns true when `token_count` exceeds 85 % of the configured max.
    pub fn should_compact(&self, token_count: usize) -> bool {
        token_count > (self.max_tokens as f64 * 0.85) as usize
    }

    /// Keep the leading system message(s) plus the most-recent N messages,
    /// where N = max_tokens / 500 (500 tokens is the per-message estimate).
    /// Oldest user/assistant pairs are dropped first.
    pub fn compact_messages(&self, messages: Vec<InferenceMessage>) -> Vec<InferenceMessage> {
        const AVG_MESSAGE_TOKENS: usize = 500;
        let keep_n = (self.max_tokens / AVG_MESSAGE_TOKENS).max(1);

        let mut system: Vec<InferenceMessage> = messages
            .iter()
            .filter(|m| m.role == "system")
            .cloned()
            .collect();

        let non_system: Vec<InferenceMessage> = messages
            .into_iter()
            .filter(|m| m.role != "system")
            .collect();

        let tail_start = non_system.len().saturating_sub(keep_n);
        system.extend(non_system.into_iter().skip(tail_start));
        system
    }

    /// Plugin identity and configuration manifest.
    pub fn manifest(&self) -> CompactManifest {
        CompactManifest {
            plugin_id: "budget-compact",
            version: "0.1.0",
            max_tokens: self.max_tokens,
        }
    }
}
