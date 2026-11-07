//! The ContextPolicy contract a strategy plugin implements.
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;

use crate::history::{HistoryItem, ItemRef};
use crate::notes::NoteSource;

/// Bumped on any incompatible change to the types in this crate.
///
/// Version 5 adds [`WindowEntry::Note`] (range-free authored working state)
/// and [`Projected::WorkingNote`](crate::Projected::WorkingNote). A strategy
/// compiled against version 4 matches window entries exhaustively and would
/// silently drop the new form, so the contract had to move.
pub const CONTRACT_VERSION: u32 = 5;

/// Body format version of a committed window (the store keeps the body
/// opaquely). A body written by an older version still decodes; a body
/// written by a newer version is refused rather than guessed at.
///
/// Version 4 bodies may carry [`WindowEntry::Note`], the range-free authored
/// working state. A version 3 reader cannot represent that variant, so such a
/// body fails its decode with a typed storage error instead of being read as a
/// window without a notebook; a version 4 reader still reads every version 3
/// body, whose entries and raw ranges are unchanged. Committed bodies are
/// immutable, so nothing is rewritten to the new version: existing windows
/// keep their bytes and keep decoding.
pub const WINDOW_BODY_VERSION: u32 = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyManifest {
    pub id: String,
    pub version: String,
    pub contract: u32,
    /// Model capabilities this strategy needs to be enabled, e.g. "vision".
    pub requires: Vec<String>,
    /// Capabilities this strategy provides while it is installed.
    #[serde(default)]
    pub provides: Vec<String>,
    /// One line a human can read in the capability list.
    #[serde(default)]
    pub summary: String,
    /// Other installed strategies this one needs at runtime.
    #[serde(default)]
    pub dependencies: Vec<String>,
}

impl Default for PolicyManifest {
    /// An empty manifest is not a strategy; it exists so a manifest can be
    /// completed field by field without repeating the whole field list.
    fn default() -> Self {
        Self {
            id: String::new(),
            version: String::new(),
            contract: CONTRACT_VERSION,
            requires: Vec::new(),
            provides: Vec::new(),
            summary: String::new(),
            dependencies: Vec::new(),
        }
    }
}

/// Actual vision envelope of the bound model, never inferred from text context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisionCaps {
    /// Advertised merged-token envelope for image requests.
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
    /// Whether this preparation answers an explicit request for a new window
    /// at the next safe boundary. A strategy may then cut a boundary window
    /// that drops nothing, which the size trigger would never justify.
    pub rollover: bool,
}

/// The bounded facts a trigger decision may use.
///
/// A trigger never reads the raw history: the decision "is a new window
/// needed" depends on the observed input size and the model's real limits,
/// not on how much history exists. Reading the history belongs to the
/// preparation, which runs off the request path.
pub struct TriggerInput<'a> {
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
    /// Authored working state of this branch (the notebook), carried with no
    /// raw range of its own.
    ///
    /// The notebook is about the whole branch, not a derivative of one range:
    /// a `from..=to` here would have to be invented, would claim raw events
    /// another entry already stands for, and would be restored away by the
    /// first rehydration of that range. A window therefore carries the note
    /// as its own entry and never counts it as covered raw events.
    ///
    /// `revision` is the journal revision the text was read from, so a
    /// committed window can be checked for carrying exactly one revision
    /// instead of a merge of two.
    Note { revision: u64, source: NoteSource, text: String, tokens: u32 },
}

impl WindowEntry {
    pub fn tokens(&self) -> u32 {
        match self {
            Self::Verbatim { tokens, .. }
            | Self::Pointer { tokens, .. }
            | Self::Text { tokens, .. }
            | Self::Image { tokens, .. }
            | Self::Note { tokens, .. } => *tokens,
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
    /// Filled at commit: tokens of the leading entries whose final input is
    /// identical to the previous window (same raw event, same derived text,
    /// same derivative bytes) under the same model/template/encoder
    /// generation. A conservative upper bound on KV reuse, not a backend
    /// cache-hit measurement; everything after it must be recomputed.
    pub reusable_prefix_tokens: u32,
    pub recompute_tokens: u32,
    /// Rendered frames this window carries.
    #[serde(default)]
    pub frames: u32,
    /// Rendered frame pixels, summed.
    #[serde(default)]
    pub frame_pixels: u64,
    /// Raw events the window's source covers. The raw events stay in the
    /// store; this is how many of them the window stands for.
    #[serde(default)]
    pub raw_events_covered: u64,
    /// Tool calls still unresolved by the execution ledger. They are kept as
    /// whole units inside the window.
    #[serde(default)]
    pub open_calls: u32,
    /// How the KV figure was obtained. It never claims a backend cache hit.
    #[serde(default)]
    pub kv_accounting: String,
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
    fn should_compact(&self, trigger: &TriggerInput<'_>) -> bool;
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
