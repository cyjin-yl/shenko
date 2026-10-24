//! The ContextPolicy contract a strategy plugin implements.
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;

use crate::history::{HistoryItem, ItemRef};

/// Bumped on any incompatible change to the types in this crate.
pub const CONTRACT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyManifest {
    pub id: String,
    pub version: String,
    pub contract: u32,
    /// Model capabilities this strategy needs to be enabled, e.g. "vision".
    pub requires: Vec<String>,
}

/// Actual vision envelope of the bound model, never inferred from text context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisionCaps {
    /// Merged-token envelope for image requests (NInfer: 32768).
    pub max_image_context: u32,
    /// Pixel edge of one vision token patch, used for estimates only.
    pub patch_px: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCaps {
    pub model_id: String,
    pub max_context: u32,
    pub vision: Option<VisionCaps>,
}

impl ModelCaps {
    /// Context limit of a request that carries images.
    pub fn image_request_context(&self) -> Option<u32> {
        self.vision.map(|v| self.max_context.min(v.max_image_context))
    }
}

/// A capability limit changes enablement, not whether the strategy exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Availability {
    Available,
    Degraded(String),
    Unavailable(String),
}

/// What the window replaces and must still match at commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowBase {
    pub source_window: EntityId,
    /// Last raw event the policy saw; later events are carried at commit.
    pub leaf_seq: u64,
    pub model_generation: u64,
    /// Installation epoch of the preparing policy; a replaced or uninstalled
    /// generation can no longer commit.
    pub policy_epoch: u64,
}

pub struct ContextInput<'a> {
    pub base: WindowBase,
    pub items: &'a [HistoryItem],
    pub caps: &'a ModelCaps,
    pub used_tokens: u32,
}

/// One element of the model-visible representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowEntry {
    /// Raw event shown unchanged, resolved through the history source.
    Verbatim { at: ItemRef, tokens: u32 },
    /// Range moved out of the window, readable on demand by seq/event id.
    Pointer { from: u64, to: u64, note: String, tokens: u32 },
    /// Derived bounded text for a range; not the raw record.
    Text { from: u64, to: u64, text: String, tokens: u32 },
    /// Rendered frame (index into `derivatives`); a lossy derivative.
    Image { from: u64, to: u64, derivative: usize, tokens: u32 },
}

impl WindowEntry {
    pub fn tokens(&self) -> u32 {
        match self {
            Self::Verbatim { tokens, .. }
            | Self::Pointer { tokens, .. }
            | Self::Text { tokens, .. }
            | Self::Image { tokens, .. } => *tokens,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Derivative {
    pub media_type: String,
    pub blake3: String,
    pub bytes: Vec<u8>,
}

impl Derivative {
    pub fn new(media_type: impl Into<String>, bytes: Vec<u8>) -> Self {
        let blake3 = blake3::hash(&bytes).to_hex().to_string();
        Self { media_type: media_type.into(), blake3, bytes }
    }
}

/// Measured or estimated costs; every field says what was actually spent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostReport {
    pub source_tokens: u32,
    pub window_tokens: u32,
    pub model_calls: u32,
    pub image_bytes: u64,
    /// Patch-count estimate, not a measured encoder count.
    pub vision_tokens_estimate: u32,
    pub render_micros: u64,
    pub truncated_chars: u64,
    pub unrenderable_chars: u64,
    /// Filled at commit: identical leading entries against the previous window.
    /// Only an identical prefix can reuse KV; everything after it is recomputed.
    pub reused_prefix_tokens: u32,
    pub computed_prefill_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedWindow {
    pub base: WindowBase,
    pub policy: PolicyManifest,
    pub entries: Vec<WindowEntry>,
    /// Calls still unresolved when the window was prepared, kept as whole units.
    pub carried_calls: Vec<String>,
    pub derivatives: Vec<Derivative>,
    pub cost: CostReport,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum PolicyError {
    #[error("strategy unavailable: {0}")]
    Unavailable(String),
    #[error("window would not shrink the context")]
    NoReduction,
    #[error("window of {window} tokens exceeds the {limit}-token request limit")]
    OverBudget { window: u32, limit: u32 },
    #[error("rendered frame unreadable: {0}")]
    FrameUnreadable(String),
    #[error("{0}")]
    Other(String),
}

/// A replaceable context strategy. Implementations are pure with respect to
/// the history: they prepare a representation and never mutate raw events.
pub trait ContextPolicy: Send + Sync + 'static {
    fn manifest(&self) -> PolicyManifest;
    fn availability(&self, caps: &ModelCaps) -> Availability;
    fn should_compact(&self, input: &ContextInput<'_>) -> bool;
    fn prepare(&self, input: &ContextInput<'_>) -> Result<PreparedWindow, PolicyError>;
}

/// Rough token estimate for derived text the policy itself produced: one per
/// CJK character, one per four other characters.
pub fn estimate_tokens(text: &str) -> u32 {
    let (mut cjk, mut other) = (0u32, 0u32);
    for c in text.chars() {
        if ('\u{2E80}'..='\u{9FFF}').contains(&c) || ('\u{AC00}'..='\u{D7AF}').contains(&c) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4)
}
