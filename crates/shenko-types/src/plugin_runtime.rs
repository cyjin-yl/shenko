//! Shared protocol types for the live plugin runtime — SPEC §18, §36, §37, §41.7, §41.8.
//!
//! These are plain data only (serde in/out), no behaviour and no dependency on
//! the kernel. They exist so the kernel's live registry, the subject's sealed
//! request plan, the Store's raw call evidence and the UI can all name the
//! same objects without one of them depending on another's crate:
//!
//! * `HookGraphBinding` — the durable identity of the hook graph bound to one
//!   request (SPEC §41.8.4). A retained call receipt must be able to say which
//!   graph and which handler artifacts produced it.
//! * `ContextContribution` and friends — one canonical contribution contract
//!   (SPEC §41.8.1) carrying slot, source, scope, expiry and budget so the
//!   provenance of transient context reaches the raw call evidence.
//! * `RuntimeView` — the read model of the live provider/capability/help
//!   registry, including its monotonic revision (SPEC §18.2 install receipts
//!   name the capability registry revision).
//!
//! §41.3 keeps shared protocol types in this layer; behaviour lives in
//! `shenko-kernel` and in the owning domain crates.

use serde::{Deserialize, Serialize};

use crate::ids::EntityId;
use crate::object::{DataRef, Digest, ObjectRef, VersionedRef};

/// Where a context contribution is rendered — SPEC §37.7.
///
/// The wire arrangement is the provider template adapter's business; the slot
/// only says which semantic region the material belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContributionSlot {
    /// Stable system/tool contract. Not replaceable by a transient contribution.
    SystemContract,
    /// The persona appended after the core rules.
    Persona,
    /// Committed task history and selected events.
    TaskHistory,
    /// Events selected for this turn.
    SelectedEvent,
    /// Dynamic tool descriptions.
    ToolDescription,
    /// Transient reminders and other prepared material valid for this request.
    TransientReminder,
}

impl ContributionSlot {
    /// Every slot of the standard profile, in render order.
    pub const ALL: [ContributionSlot; 6] = [
        ContributionSlot::SystemContract,
        ContributionSlot::Persona,
        ContributionSlot::TaskHistory,
        ContributionSlot::SelectedEvent,
        ContributionSlot::ToolDescription,
        ContributionSlot::TransientReminder,
    ];

    /// True when a transient contribution may be written into this slot.
    ///
    /// `SystemContract` is owned by the kernel contract; a plugin that tries to
    /// write it is a write violation, not a merged value.
    pub fn accepts_transient(self) -> bool {
        !matches!(self, ContributionSlot::SystemContract)
    }
}

/// What kind of thing produced a contribution — SPEC §45.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContributionKind {
    /// A sourced, time/versioned observation.
    Perception,
    /// A reusable connection, question or intuition.
    Awareness,
    /// A method for accomplishing a class of work.
    Skill,
    /// An execution or selection strategy.
    Policy,
    /// Code that obtains an observation or performs an effect.
    Code,
    /// A transient nudge prepared for one request.
    Reminder,
    /// A plain fact with a source.
    Fact,
}

/// Who produced a contribution, bound to the actual implementation artifact.
///
/// The artifact digest is what makes the provenance checkable: an
/// acknowledgement unit or a plugin build is named by its content, not by its
/// label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributionSource {
    pub plugin_id: EntityId,
    pub plugin_version: String,
    pub artifact_digest: Option<Digest>,
    pub kind: ContributionKind,
}

/// Applicability scope of one prepared contribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributionScope {
    pub scope_id: EntityId,
    pub task_id: Option<EntityId>,
}

/// How long a prepared contribution stays usable — SPEC §41.8.1 `lifetime`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ContributionLifetime {
    /// Valid for exactly the request that used it.
    ThisRequest,
    /// Valid until the given inbox event revision is reached.
    UntilEventRevision { event_revision: u64 },
    /// Valid until the given wall-clock instant (milliseconds since epoch).
    ExpiresAtMillis { expires_at_ms: i64 },
}

impl ContributionLifetime {
    /// Wall-clock expiry. A lifetime bound to a request or an event revision
    /// has no wall-clock end of its own.
    pub fn is_expired(&self, now_ms: i64) -> bool {
        match self {
            Self::ThisRequest => false,
            Self::UntilEventRevision { .. } => false,
            Self::ExpiresAtMillis { expires_at_ms } => now_ms >= *expires_at_ms,
        }
    }

    /// Whether the contribution is consumed by the first request that uses it.
    pub fn is_single_use(&self) -> bool {
        matches!(self, Self::ThisRequest)
    }

    /// The wall-clock end of this lifetime, when it has one.
    pub fn expires_at_ms(&self) -> Option<i64> {
        match self {
            Self::ExpiresAtMillis { expires_at_ms } => Some(*expires_at_ms),
            _ => None,
        }
    }
}

/// SPEC §41.8.1 `ContextContribution` — the one canonical definition.
///
/// A contribution is *prepared* material with a source, an applicable scope, an
/// expiry and a token budget. It is not a hook result: a hook returns
/// contributions, the context policy merges them, and the sealed request names
/// exactly what was sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextContribution {
    /// Stable key inside its slot. The merge rule is per (slot, key).
    pub key: String,
    pub slot: ContributionSlot,
    pub source: ContributionSource,
    pub scope: ContributionScope,
    /// The input revision this contribution was prepared against.
    pub input_revision: String,
    pub lifetime: ContributionLifetime,
    /// A required contribution cannot be skipped as optional.
    pub required: bool,
    /// Token budget the producer asked for.
    pub budget_tokens: u64,
    /// Tokens the prepared material actually occupies.
    pub tokens: u64,
    /// The prepared bytes.
    pub data: DataRef,
    pub prepared_at_ms: i64,
}

impl ContextContribution {
    /// True when this contribution may no longer be used.
    pub fn is_expired(&self, now_ms: i64) -> bool {
        self.lifetime.is_expired(now_ms)
    }

    /// The wall-clock end of this contribution's lifetime, when it has one.
    pub fn expires_at_ms(&self) -> Option<i64> {
        self.lifetime.expires_at_ms()
    }

    /// True when the first request that uses this contribution consumes it.
    pub fn single_use(&self) -> bool {
        self.lifetime.is_single_use()
    }
}

/// One contribution that was prepared but not sent, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributionOmission {
    pub key: String,
    pub slot: ContributionSlot,
    pub source: ContributionSource,
    /// Why it was not sent: expired, over budget, superseded, not selected…
    pub reason: String,
}

/// Exact per-request usage evidence — SPEC §37.7, §41.8.
///
/// This is the record that lets the subject (and the raw call evidence) say
/// which transient material actually reached the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributionUsage {
    pub request_id: EntityId,
    pub request_revision: u64,
    pub input_revision: String,
    pub seal_digest: Digest,
    /// Complete prepared identities, not merely slot/key: consuming one
    /// scope or producer revision must not spend another's material.
    pub sent: Vec<ContextContribution>,
    pub omitted: Vec<ContributionOmission>,
    pub total_tokens: u64,
}

/// Durable identity of the hook graph bound to one request — SPEC §41.8.4.
///
/// `graph_digest` is a content digest over the admitted point contracts and
/// the ordered member specifications (each including its implementation
/// artifact). Two graphs with the same digest dispatch identically, so a
/// retained receipt stays interpretable across a hot update and a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookGraphBinding {
    /// blake3 digest over the admitted contracts and ordered member specs.
    pub graph_digest: Digest,
    /// Runtime-only generation of the admitted graph.
    pub revision: u64,
    /// Admitted hook point ids, in dispatch order.
    pub point_ids: Vec<String>,
    /// Implementation artifact digest of each ordered handler.
    pub handler_digests: Vec<Digest>,
    /// In-flight lease counter observed when the graph was bound.
    pub lease_epoch: u64,
}

/// Health of one registered provider — SPEC §18.2.
///
/// Installed, healthy, discoverable, verified-for-model and degraded are
/// distinct facts; a successful load is not completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHealth {
    /// Registered but not yet probed.
    Installed,
    /// Probed and usable.
    Healthy,
    /// Usable but not discoverable by the subject's index yet.
    Degraded,
    /// Known to be unusable.
    Unavailable,
}

/// One operation of a provider with its §36 effect contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationEffectView {
    pub operation_name: String,
    /// The effect contract of this one operation.
    pub effect_contract: crate::effect::EffectContract,
}

/// One registered provider — the live projection of an installed capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderView {
    pub provider_id: String,
    /// Provider family, e.g. "node.election", "model.inference", "channel.qq".
    pub kind: String,
    pub version: String,
    pub plugin_id: EntityId,
    pub implementation: VersionedRef,
    pub description: String,
    pub operations: Vec<OperationEffectView>,
    pub capabilities: Vec<String>,
    pub health: ProviderHealth,
}

/// One registered capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityView {
    pub capability_id: String,
    pub version: String,
    pub description: String,
    pub plugin_id: EntityId,
    pub provider_id: Option<String>,
    pub enabled: bool,
}

/// One capability a native task definition offers.
///
/// A native definition declares what it can actually execute while it is
/// installed. The declaration is bound to the implementation artifact, so the
/// capability view names the implementation that was actually admitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeCapabilityDeclaration {
    pub capability_id: String,
    pub version: String,
    pub description: String,
    /// Model-facing usage text. It is valid only while this definition is
    /// installed and is never a permission.
    pub usage: Option<String>,
}

/// One registered help document — SPEC §18.2 usability package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelpView {
    pub capability_id: String,
    pub version: String,
    /// What the capability helps to accomplish.
    pub purpose: String,
    /// The key methods and why they are useful.
    pub key_methods: Vec<String>,
    /// When the guidance holds.
    pub applies_when: Vec<String>,
    /// When it does not.
    pub does_not_apply_when: Vec<String>,
    /// Schema and version help.
    pub schema_help: Option<String>,
    /// Known limits of this exact version.
    pub limitations: Vec<String>,
    /// How to recover when it fails.
    pub recovery: Option<String>,
    pub implementation: VersionedRef,
}

/// The read model of the live runtime registry.
///
/// `revision` increases on every registration, withdrawal or health change, so
/// an install receipt can name the exact registry state it observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeView {
    pub revision: u64,
    pub providers: Vec<ProviderView>,
    pub capabilities: Vec<CapabilityView>,
    pub help: Vec<HelpView>,
}

/// Reference to one sealed contribution payload inside a request's evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributionBinding {
    pub contribution: ContextContribution,
    /// The sealed object the bytes were read back from.
    pub object: ObjectRef,
}
