//! Shared protocol types for the live plugin runtime.
//!
//! These are plain data only (serde in/out), no behaviour and no dependency on
//! the kernel. They exist so the kernel's live registry, the subject's sealed
//! request plan, the Store's raw call evidence and the UI can all name the
//! same objects without one of them depending on another's crate:
//!
//! * `HookGraphBinding` — the durable identity of the hook graph bound to one
//!   request. A retained call receipt must be able to say which
//!   graph and which handler artifacts produced it.
//! * `ContextContribution` and friends — one canonical contribution contract
//!   carrying slot, source, scope, expiry and budget so the
//!   provenance of transient context reaches the raw call evidence.
//! * `RuntimeView` — the read model of the live provider/capability/help
//!   registry, including its monotonic revision (install receipts
//!   name the capability registry revision).
//!
//! keeps shared protocol types in this layer; behaviour lives in
//! `shenko-kernel` and in the owning domain crates.

use serde::{Deserialize, Serialize};

use crate::ids::EntityId;
use crate::object::{DataRef, Digest, ObjectRef, VersionedRef};

/// Where a context contribution is rendered.
///
/// The wire arrangement is the provider template adapter's business; the slot
/// only says which semantic region the material belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

/// What kind of thing produced a contribution.
///
/// This is a producer hint, not a storage or knowledge classification: fact
/// and conclusion are readings of where a reason terminates, not two kinds
/// `SourcedContent` therefore names material that carries its source,
/// whatever that reading turns out to be.
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
    /// Content that carries its source and revision.
    ///
    /// A record sealed before this cutover spells this kind `fact` in its
    /// immutable bytes; readers of those records run
    /// [`migrate_legacy_contribution_kinds`] instead of the enum accepting a
    /// legacy alias forever.
    SourcedContent,
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

/// The serde label [`ContributionKind::SourcedContent`] carried in records
/// sealed before the cutover.
pub const LEGACY_SOURCED_CONTENT_LABEL: &str = "fact";

/// Migration reader for immutable records sealed before the `sourced_content` rename.
///
/// Writers only ever emit `sourced_content`, and [`ContributionKind`] has no
/// legacy alias: a record that still says `fact` fails to decode rather than
/// being silently re-read as the current spelling. A reader that must
/// interpret such a record runs this migration over the parsed document
/// first. It rewrites only a contribution source — an object pairing `kind`
/// with its producer identity `plugin_id` — so an unrelated `kind` tag (a
/// contribution lifetime's discriminant, for example) is never touched, and
/// it never rewrites sealed bytes: the caller migrates its parsed copy.
///
/// Returns how many labels were migrated.
pub fn migrate_legacy_contribution_kinds(document: &mut serde_json::Value) -> usize {
    fn walk(value: &mut serde_json::Value) -> usize {
        match value {
            serde_json::Value::Object(map) => {
                let mut migrated = 0;
                let legacy_source = map.contains_key("plugin_id")
                    && map.get("kind").and_then(serde_json::Value::as_str)
                        == Some(LEGACY_SOURCED_CONTENT_LABEL);
                if legacy_source {
                    map.insert(
                        "kind".into(),
                        serde_json::Value::String("sourced_content".into()),
                    );
                    migrated += 1;
                }
                for (_, child) in map.iter_mut() {
                    migrated += walk(child);
                }
                migrated
            }
            serde_json::Value::Array(items) => items.iter_mut().map(walk).sum(),
            _ => 0,
        }
    }
    walk(document)
}

/// Applicability scope of one prepared contribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributionScope {
    pub scope_id: EntityId,
    pub task_id: Option<EntityId>,
}

/// How long a prepared contribution stays usable — `lifetime`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ContributionLifetime {
    /// Valid for exactly the request that used it.
    ThisRequest,
    /// Consumed by its first request, or expired at the deadline if unused.
    ThisRequestUntilMillis { expires_at_ms: i64 },
    /// Valid until the given inbox event revision is reached.
    UntilEventRevision { event_revision: u64 },
    /// Valid until the given wall-clock instant (milliseconds since epoch).
    ExpiresAtMillis { expires_at_ms: i64 },
}

impl ContributionLifetime {
    /// Wall-clock expiry. Request/event-only lifetimes have no deadline;
    /// the bounded single-request variant retains both termination rules.
    pub fn is_expired(&self, now_ms: i64) -> bool {
        match self {
            Self::ThisRequest => false,
            Self::UntilEventRevision { .. } => false,
            Self::ThisRequestUntilMillis { expires_at_ms }
            | Self::ExpiresAtMillis { expires_at_ms } => now_ms >= *expires_at_ms,
        }
    }

    /// Whether the contribution is consumed by the first request that uses it.
    pub fn is_single_use(&self) -> bool {
        matches!(self, Self::ThisRequest | Self::ThisRequestUntilMillis { .. })
    }

    /// The wall-clock end of this lifetime, when it has one.
    pub fn expires_at_ms(&self) -> Option<i64> {
        match self {
            Self::ThisRequestUntilMillis { expires_at_ms }
            | Self::ExpiresAtMillis { expires_at_ms } => Some(*expires_at_ms),
            _ => None,
        }
    }
}

/// `ContextContribution` — the one canonical definition.
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

/// Exact per-request usage evidence.
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

/// Durable identity of the hook graph bound to one request.
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

/// Health of one registered provider.
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

/// One operation of a provider with its effect contract.
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

/// One registered help document — usability package.
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

// ---------------------------------------------------------- root lifecycle ---
//
// One plugin-root installation is one exact runtime implementation
// (`plugin_id` + `implementation`) installed for one exact Node unit version.
// Its lifecycle has four durable phases:
//
//   prepared  — the artifact identity is known and retained; nothing is
//               published and no new call may be admitted.
//   published — the installation is durably admitted for this runtime; the
//               publication receipt names the registry state observed at that
//               moment. Still no live attachment.
//   attached  — a real process generation attached the publication to the
//               live runtime root, so new calls may use it. A restart appends
//               a new attachment admission for the same publication; the old
//               attachment receipt remains the fact of the dead generation.
//   retired   — new calls stop. The installation's artifact, operation
//               contracts and evidence receipts stay readable so an old call
//               can still be reconciled against the implementation it froze.
//
// A *new process generation* re-attaches; it does not republish or revive a
// retired generation. Re-enabling the same exact installation opens the next
// generation explicitly and keeps the previous retirement admission immutable.

use crate::commit::CommitRef;
use crate::ids::RuntimeGeneration;

/// Durable phase of one plugin-root installation generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginRootPhase {
    Prepared,
    Published,
    Attached,
    Retired,
}

impl PluginRootPhase {
    pub fn name(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Published => "published",
            Self::Attached => "attached",
            Self::Retired => "retired",
        }
    }

    /// Terminal for this generation. A later generation is a new record
    /// generation, never a transition out of `retired`.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Retired)
    }

    /// New calls may only use an *attached* phase, and only in the process
    /// generation that recorded the attachment. Publication alone is not
    /// enough: it authorizes attachment, not execution.
    pub fn admits_new_calls(self) -> bool {
        matches!(self, Self::Attached)
    }

    /// The durable phase order of one generation.
    ///
    /// `Retired` is reachable from every non-terminal phase: an installation
    /// that was only prepared or published can be withdrawn without ever
    /// having executed, and that retirement states `effect_status=none`
    /// rather than implying an external stop it never performed.
    /// `Attached → Attached` is not a phase transition: it is the same phase
    /// with a new attachment admission for a new process generation.
    pub fn can_advance_to(self, next: PluginRootPhase) -> bool {
        use PluginRootPhase::*;
        match (self, next) {
            (Prepared, Published)
            | (Prepared, Retired)
            | (Published, Attached)
            | (Published, Retired)
            | (Attached, Retired) => true,
            _ => false,
        }
    }
}

/// Exact identity of one plugin-root installation.
///
/// Two artifacts of the same plugin are two installations; two process
/// generations of one installation are generations of the same record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRootIdentity {
    /// Node installation unit this runtime implementation was installed from.
    pub unit_id: String,
    pub unit_revision: String,
    pub unit_artifact_digest: Digest,
    /// The runtime registration identity (the kernel's `plugin_id`).
    pub plugin_id: EntityId,
    /// The exact implementation artifact and symbol this generation executes.
    pub implementation: VersionedRef,
    /// Provider ids this installation publishes, when already known.
    #[serde(default)]
    pub provider_ids: Vec<String>,
    /// Capability ids this installation publishes, when already known.
    #[serde(default)]
    pub capability_ids: Vec<String>,
}

impl PluginRootIdentity {
    /// Deterministic installation identity over the exact identity tuple.
    ///
    /// Provider/capability lists are sorted before hashing so the same
    /// registration set always names the same installation, independent of
    /// observation order.
    pub fn installation_id(&self) -> EntityId {
        let mut providers = self.provider_ids.clone();
        providers.sort();
        providers.dedup();
        let mut capabilities = self.capability_ids.clone();
        capabilities.sort();
        capabilities.dedup();
        let canonical = serde_json::json!([
            "shenko.plugin-root.identity/v1",
            self.unit_id,
            self.unit_revision,
            self.unit_artifact_digest.0,
            self.plugin_id.to_string(),
            self.implementation.artifact.digest.0,
            self.implementation.artifact.locator,
            self.implementation.symbol,
            self.implementation.schema.name,
            self.implementation.schema.major,
            self.implementation.schema.digest.0,
            providers,
            capabilities,
        ]);
        EntityId::derived(
            "shenko.plugin-root.installation/v1",
            canonical.to_string().as_bytes(),
        )
    }
}

/// How the artifact identity was retained before the phase was admitted.
///
/// This is evidence classification, not a promise that the bytes were read
/// again: `StoreCas` is verified by the Store itself at admission; the
/// `InstallationUnit` variant is the caller's observation of the Node's own
/// installation fact, and the receipt says so instead of pretending the Store
/// inspected those bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PluginArtifactBinding {
    /// The implementation bytes are a local CAS object the Store verifies.
    StoreCas { object: ObjectRef },
    /// The bytes belong to the Node installation unit named by the identity.
    InstallationUnit { retained: bool },
}

impl PluginArtifactBinding {
    /// Whether the binding asserts the artifact is still addressable.
    pub fn retained(&self) -> bool {
        match self {
            Self::StoreCas { .. } => true,
            Self::InstallationUnit { retained } => *retained,
        }
    }
}

/// One durable admission of a phase transition.
///
/// The admission id is derived from `(installation, generation, phase,
/// runtime generation)`, so a retry of the same phase in the same process
/// generation returns the original receipt instead of appending a second one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginPhaseAdmission {
    pub schema_version: u32,
    pub admission_id: EntityId,
    pub installation_id: EntityId,
    /// Generation of the installation record this admission belongs to.
    pub generation: u64,
    pub phase: PluginRootPhase,
    /// Record revision this admission produced.
    pub record_revision: u64,
    /// Process generation that observed or performed this phase.
    pub runtime_generation: RuntimeGeneration,
    /// Live registry revision observed with the publication/attachment.
    pub observed_registry_revision: u64,
    /// Live capability registry revision observed with the publication.
    pub observed_capability_revision: u64,
    /// Hook graph digest observed at publication, when the root published one.
    pub observed_hook_graph_digest: Option<Digest>,
    /// The artifact binding this phase was admitted under.
    pub artifact: PluginArtifactBinding,
    /// Why this phase was admitted.
    pub reason: String,
    /// Caller-observed evidence for this phase. These are references, not a
    /// claim that the Store re-read their bytes.
    #[serde(default)]
    pub evidence: Vec<ObjectRef>,
    /// Unresolved operations counted from the Store's own operation rows at a
    /// retirement commit. `None` for every other phase. The count is kept on
    /// the retirement admission because a later generation replaces the
    /// installation record's current view.
    #[serde(default)]
    pub unresolved_operations: Option<u64>,
    pub committed_at: CommitRef,
}

/// One plugin-root installation record (per installation, not per generation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRootRecord {
    pub schema_version: u32,
    pub installation_id: EntityId,
    pub identity: PluginRootIdentity,
    /// Current generation. The first installation is generation 0.
    pub generation: u64,
    pub phase: PluginRootPhase,
    pub record_revision: u64,
    pub prepared: PluginPhaseAdmission,
    pub published: Option<PluginPhaseAdmission>,
    pub attached: Option<PluginPhaseAdmission>,
    pub retired: Option<PluginPhaseAdmission>,
    /// Process generation of the latest attachment, when attached.
    pub attached_runtime_generation: Option<RuntimeGeneration>,
    /// Whether the artifact was addressable when the current generation was
    /// prepared and at retirement.
    pub artifact_retained: bool,
    /// Whether operation contracts, evidence receipts and the installation
    /// record itself remain readable after retirement. Retirement never
    /// deletes them, so this is true even when the artifact bytes are gone.
    pub evidence_retained: bool,
    /// Operations still unresolved at the retirement commit, counted from the
    /// Store's own operation rows for this exact plugin/artifact. `None`
    /// before retirement.
    pub in_flight_operations_at_retirement: Option<u64>,
    pub prepared_commit: CommitRef,
    pub updated_commit: CommitRef,
}

impl PluginRootRecord {
    pub fn installation_id(&self) -> EntityId {
        self.installation_id
    }

    pub fn admits_new_calls(&self, current: &RuntimeGeneration) -> bool {
        self.phase.admits_new_calls()
            && self
                .attached_runtime_generation
                .as_ref()
                .is_some_and(|attached| attached == current)
    }
}

/// Caller request for the prepare phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginPrepareRequest {
    /// Phase is admitted for this process generation (recorded in the receipt).
    pub runtime_generation: RuntimeGeneration,
    pub artifact: PluginArtifactBinding,
    pub reason: String,
    #[serde(default)]
    pub evidence: Vec<ObjectRef>,
}

/// Caller request for the publish phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginPublishRequest {
    pub runtime_generation: RuntimeGeneration,
    /// Registry revision this publication was prepared against.
    pub observed_registry_revision: u64,
    /// Capability catalog revision this publication was prepared against.
    pub observed_capability_revision: u64,
    pub observed_hook_graph_digest: Option<Digest>,
    pub reason: String,
    #[serde(default)]
    pub evidence: Vec<ObjectRef>,
}

/// Caller request for the attach phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginAttachRequest {
    /// The process generation that actually attached the publication.
    pub runtime_generation: RuntimeGeneration,
    pub observed_registry_revision: u64,
    pub observed_capability_revision: u64,
    pub reason: String,
    #[serde(default)]
    pub evidence: Vec<ObjectRef>,
}

/// Caller request for the retire phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRetireRequest {
    pub runtime_generation: RuntimeGeneration,
    pub reason: String,
    /// The caller's observation that the Node installation still retains the
    /// artifact. The Store's own CAS check wins for `StoreCas` bindings.
    pub artifact_retained: bool,
    #[serde(default)]
    pub evidence: Vec<ObjectRef>,
}

/// Stable refusal codes of the durable lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginLifecycleRefusalCode {
    /// The same installation identity arrived with a different definition.
    IdentityConflict,
    /// A concurrent writer committed a different record revision first.
    RevisionConflict,
    /// The phase cannot follow the committed phase.
    IllegalPhaseTransition,
    /// `attach` was asked for while the installation was never published.
    AttachBeforePublish,
    /// The installation generation is retired; new calls are refused.
    Retired,
    /// The artifact is no longer addressable, so it cannot be published or
    /// reopened for a new generation.
    ArtifactUnavailable,
    /// No durable record exists for this installation identity.
    UnknownInstallation,
    /// The same phase was admitted in this process generation with a
    /// different observation (a second, inconsistent receipt).
    AdmissionConflict,
}

impl PluginLifecycleRefusalCode {
    pub fn code(self) -> &'static str {
        match self {
            Self::IdentityConflict => "PLUGIN_ROOT_IDENTITY_CONFLICT",
            Self::RevisionConflict => "PLUGIN_ROOT_REVISION_CONFLICT",
            Self::IllegalPhaseTransition => "PLUGIN_ROOT_ILLEGAL_PHASE",
            Self::AttachBeforePublish => "PLUGIN_ATTACH_BEFORE_PUBLISH",
            Self::Retired => "PLUGIN_ROOT_RETIRED",
            Self::ArtifactUnavailable => "PLUGIN_ARTIFACT_UNAVAILABLE",
            Self::UnknownInstallation => "PLUGIN_ROOT_UNKNOWN_INSTALLATION",
            Self::AdmissionConflict => "PLUGIN_ROOT_ADMISSION_CONFLICT",
        }
    }
}

/// Typed lifecycle refusal. Callers downcast this instead of matching prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginLifecycleRefusal {
    pub code: PluginLifecycleRefusalCode,
    pub installation_id: Option<EntityId>,
    pub phase: Option<PluginRootPhase>,
    pub detail: String,
}

impl PluginLifecycleRefusal {
    pub fn new(code: PluginLifecycleRefusalCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            installation_id: None,
            phase: None,
            detail: detail.into(),
        }
    }

    pub fn for_installation(
        mut self,
        installation_id: EntityId,
        phase: Option<PluginRootPhase>,
    ) -> Self {
        self.installation_id = Some(installation_id);
        self.phase = phase;
        self
    }
}

impl std::fmt::Display for PluginLifecycleRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.code(), self.detail)
    }
}

impl std::error::Error for PluginLifecycleRefusal {}

/// What restart recovery may do for one committed phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PluginRecoveryAction {
    /// Prepared but never published: verify the retained artifact and publish.
    /// No new call can exist, because publication never committed.
    Publish,
    /// Published but never attached in this process: attach. Publication is
    /// durable, so this does not republish.
    Attach,
    /// Attached, but the attachment belongs to a dead process generation:
    /// reattach under the current generation and reconcile unresolved
    /// operations by their frozen implementation identity before admitting
    /// new calls.
    Reattach {
        previous_runtime_generation: RuntimeGeneration,
    },
    /// Attached in the current process generation: nothing to resume.
    Ready,
    /// Retired: no new calls. Old operations and receipts stay resolvable
    /// against the retained artifact identity.
    Retained,
}

/// The recovery decision for one installation, derived only from committed
/// phases and the current process generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRecoveryPlan {
    pub schema_version: u32,
    pub installation_id: EntityId,
    pub generation: u64,
    pub phase: PluginRootPhase,
    pub current_runtime_generation: RuntimeGeneration,
    pub action: PluginRecoveryAction,
    /// New calls may use this installation only when this is true.
    pub admits_new_calls: bool,
    /// Unresolved operations currently recorded for this exact
    /// plugin/artifact (prepared, in flight or reconciling).
    pub unresolved_operations: u64,
    pub artifact_retained: bool,
    pub evidence_retained: bool,
}

/// Whether an old call's exact implementation is available right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PluginImplementationAvailability {
    /// Attached in the current process generation.
    Live { runtime_generation: RuntimeGeneration },
    /// Attached in an earlier process generation; the current process has not
    /// reattached yet. Not executable now.
    AwaitingReattach {
        previous_runtime_generation: RuntimeGeneration,
    },
    /// Published but not attached; never live in this process.
    AwaitingAttach,
    /// Prepared only. Publication never committed, so no attach is legal.
    NotPublished,
    /// Retired. The artifact and evidence are retained; the implementation is
    /// not loaded, and a re-execution needs an explicit new generation.
    RetainedOnly { retired_commit: CommitRef },
    /// No durable installation record matches the call's frozen contract.
    Missing,
}

/// Resolution of one frozen operation against the durable lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginCallResolution {
    pub operation_id: EntityId,
    pub plugin_id: EntityId,
    pub artifact_digest: Digest,
    pub availability: PluginImplementationAvailability,
    /// Evidence receipts are never deleted by retirement, so a retained
    /// operation keeps an auditable record.
    pub receipts_readable: bool,
    pub receipt_count: u64,
}
