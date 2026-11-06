//! Atomic notices that the forgetting plugin receives from the subject loop
//! and other domains.
//!
//! A signal is a mechanism record — it says "the subject referenced this
//! content at this active-runtime instant" or "a retrieval returned this
//! digest". The signal batch is the input to a stage-advance evaluation;
//! the forgetting formula itself lives in revisable definition content.

use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;

/// One atomic notice about content usage or reference.
///
/// Signals are additive mechanism records. They cannot be retracted or
/// updated: a wrong signal is corrected by recording a new one with a
/// different kind, not by editing the old one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgettingSignal {
    pub signal_id: EntityId,
    pub agent_id: EntityId,
    /// The content this signal is about.
    pub digest: Digest,
    pub kind: ForgettingSignalKind,
    /// The wall-clock timestamp of the signal, taken from the same clock as
    /// the active-runtime segments. Signals outside any measured segment are
    /// still recorded: the subject may reference content while paused, and
    /// that reference is real even though no active time elapsed.
    pub recorded_at_ms: i64,
    /// The step during which this signal was generated, if inside a step.
    #[serde(default)]
    pub step_id: Option<EntityId>,
}

/// What happened to the content.
///
/// The kind is a classification of the event, not a weight or a score.
/// How much each kind affects the forgetting clock is part of the revisable
/// definition content, not hard-coded here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgettingSignalKind {
    /// The content appeared in the subject's projected context (prompt).
    ContextProjection,
    /// The subject explicitly referenced or quoted this content in output.
    ExplicitReference,
    /// A retrieval or recall query returned this content.
    RetrievalHit,
    /// The content was part of a tool invocation or its result.
    ToolUsage,
    /// The subject revised this content (new version created).
    Revision,
    /// An external event mentioned or updated this content.
    ExternalMention,
    /// Storage-pressure notice: the content is consuming capacity.
    StoragePressure,
}

impl ForgettingSignalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContextProjection => "context_projection",
            Self::ExplicitReference => "explicit_reference",
            Self::RetrievalHit => "retrieval_hit",
            Self::ToolUsage => "tool_usage",
            Self::Revision => "revision",
            Self::ExternalMention => "external_mention",
            Self::StoragePressure => "storage_pressure",
        }
    }
}

/// A batch of signals to evaluate in one forgetting cycle.
#[derive(Debug, Clone, Default)]
pub struct SignalBatch {
    pub signals: Vec<ForgettingSignal>,
}

impl SignalBatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, signal: ForgettingSignal) {
        self.signals.push(signal);
    }

    /// All distinct content digests referenced in this batch.
    pub fn distinct_digests(&self) -> Vec<Digest> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        for signal in &self.signals {
            if seen.insert(signal.digest.clone()) {
                result.push(signal.digest.clone());
            }
        }
        result
    }

    /// Signals for one specific content digest.
    pub fn signals_for(&self, digest: &Digest) -> Vec<&ForgettingSignal> {
        self.signals
            .iter()
            .filter(|signal| &signal.digest == digest)
            .collect()
    }
}
