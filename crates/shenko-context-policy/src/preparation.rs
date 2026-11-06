//! The prepared-window publication contract — SPEC §12.3.2.
//!
//! A preparation runs off the request path: it reads the raw history and
//! renders the new representation while the subject keeps thinking. The
//! result is a *candidate* bound to the revisions it was prepared against —
//! the branch scope, the window it started from, the raw leaf it read, the
//! model generation and the installation epoch of the strategy that prepared
//! it. Publishing checks that binding against the live state, so a window
//! prepared for a moved head, a replaced strategy or another branch can
//! never be committed.
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;

use crate::history::StorageError;
use crate::policy::{ModelCaps, PolicyManifest, PreparedWindow};
use crate::registry::Attempt;

/// The revisions a prepared window is bound to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparationBinding {
    /// The branch/scope the window belongs to.
    pub scope_id: EntityId,
    /// The window the preparation started from.
    pub source_window: EntityId,
    /// The last raw event the preparation read. Events that arrive later are
    /// carried verbatim by materialization; they are never dropped.
    pub leaf_seq: u64,
    /// The model/template/encoder generation the preparation was made for.
    pub model_generation: u64,
    /// The installation epoch of the preparing strategy.
    pub policy_epoch: u64,
    /// The strategy that prepared the window.
    pub policy_id: String,
}

/// A prepared window waiting to be published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedCandidate {
    pub binding: PreparationBinding,
    pub policy: PolicyManifest,
    pub window: PreparedWindow,
    /// What the strategies reported while preparing.
    pub attempts: Vec<Attempt>,
    /// The model limits the window was prepared under. The capacity check at
    /// adoption uses exactly these limits.
    pub caps: ModelCaps,
    pub prepared_at_ms: i64,
    /// Whether this preparation answered an explicit request for a new
    /// window at the next safe boundary, rather than the size trigger. The
    /// request is consumed by the attempt that reports it here.
    #[serde(default)]
    pub rollover: bool,
}

impl PreparedCandidate {
    /// The request limit this candidate was prepared for: an image-carrying
    /// window is bound to the image envelope, a text window to the text
    /// context.
    pub fn request_limit(&self) -> u32 {
        if self.window.derivatives.is_empty() {
            self.caps.max_context
        } else {
            self.caps.image_request_context().unwrap_or(0)
        }
    }
}

/// Why a candidate could not be published.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum PublishError {
    #[error("prepared window belongs to scope {prepared}, this chain is {expected}")]
    ScopeMismatch { expected: EntityId, prepared: EntityId },
    #[error("window was prepared against {prepared}, but {current} is current")]
    StaleSource { prepared: EntityId, current: EntityId },
    #[error("model generation moved from {prepared} to {current}")]
    ModelGenerationChanged { prepared: u64, current: u64 },
    #[error("policy {policy} epoch {epoch} is no longer installed")]
    StalePolicy { policy: String, epoch: u64 },
    #[error("prepared leaf {prepared} is ahead of the history leaf {leaf:?}")]
    LeafAhead { prepared: u64, leaf: Option<u64> },
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// What happened to the last preparation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparationOutcome {
    /// The candidate was published and is waiting for adoption.
    Published,
    /// The candidate was adopted and committed.
    Adopted,
    /// The candidate no longer matched the live revisions and was dropped.
    Stale,
    /// The candidate was refused (capacity, budget, unreadable frame).
    Refused,
}

/// The visible outcome of one preparation, kept for honest reporting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparationDiagnostic {
    pub policy: String,
    pub outcome: PreparationOutcome,
    pub detail: String,
    pub at_ms: i64,
}

/// The observed input size that made a compaction worth preparing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PressureObservation {
    pub used_tokens: u32,
    pub max_context: u32,
    pub model_id: String,
    /// The exact model limits the observation was made under. A preparation
    /// that runs later uses these limits, so it prepares for the model that
    /// was actually observed instead of a re-derived guess.
    pub caps: ModelCaps,
    pub observed_at_ms: i64,
}

/// What a configured preference order actually became.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreferenceAdoption {
    /// The order that was requested.
    pub requested: Vec<String>,
    /// The order in effect: the requested ids that are installed, in the
    /// requested order, followed by the remaining installed ids.
    pub adopted: Vec<String>,
    /// Requested ids that are not installed.
    pub unknown: Vec<String>,
    pub adopted_at_ms: i64,
}
