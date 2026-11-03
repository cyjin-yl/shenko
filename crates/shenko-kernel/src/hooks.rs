//! Hook API — SPEC §37, §41.8.
//!
//! The registry owns a *named, versioned* set of hook points. A point declares
//! its contract (major version, input/output schema, mode, writable fields,
//! finite per-handler budget and failure policy) and the graph admits handlers
//! against that contract. A handler compiled for a different major is fenced
//! instead of being reinterpreted, and a leased snapshot keeps dispatching
//! under the contract it took.
//!
//! Every mode of §41.8.2 is implemented with its own output synthesis:
//!
//! | mode | legal outputs | synthesis |
//! | --- | --- | --- |
//! | observe | `no_change`, `defer` | nothing is changed; an illegal verdict is discarded with a diagnostic |
//! | transform | `no_change`, `replace`, `defer`, `reject` | serial application of the declared patch rule; a write outside `writable_paths` is a violation and is never adopted |
//! | collect | `no_change`, `contribute`, `defer`, `reject` | merge by `(slot, key)` through one shared rule; a disagreement is `CONTRIBUTION_CONFLICT` and no producer wins by arriving later |
//! | decide | `no_change`, `decide`, `defer`, `reject` | the first valid decision ends the point |
//! | around | the point's own outputs | the handler may call `next()` at most once; the type system, not a runtime flag, enforces it |
//!
//! Ordering is a topological sort of the declared `before`/`after` edges with
//! stable handler ids as the tie break, so installation order, async completion
//! order and message urgency never change the result. A candidate graph with a
//! cycle, a missing hard dependency or an incompatible schema is refused and
//! the running graph stays in force.
//!
//! In-flight work leases the graph it started with. `HookRegistry` counts the
//! live leases per revision so a teardown can wait for the real barrier
//! instead of guessing.
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
use shenko_types::object::{DataRef, Digest, SchemaRef, VersionedRef};
use shenko_types::plugin_runtime::{ContextContribution, ContributionSlot, HookGraphBinding};
use thiserror::Error;

use crate::contribution::{merge_contributions, ContributionConflict};

/// Payload schema the registry currently admits. A hook compiled against a
/// different schema is refused instead of being interpreted against new
/// fields.
pub const HOOK_SCHEMA_VERSION: u32 = 2;

/// Major version of every built-in hook point in the standard install profile.
/// Handlers declare the point major they were compiled against; a mismatch is
/// refused at admission instead of being interpreted against a newer contract.
pub const STANDARD_HOOK_POINT_MAJOR: u32 = 1;

/// Per-handler budget of the standard install profile. SPEC §41.8 gives
/// optional contributions a bounded local preparation budget; an observer
/// hook that does not answer inside it is diagnosed, never awaited forever.
pub const STANDARD_HOOK_TIMEOUT_MS: u64 = 250;

/// A hook point identity: `namespace.name`.
///
/// The namespace says who owns the timing, the name says which moment it is.
/// A point is not a global stage: two providers may own different points with
/// the same local name, and a plugin may introduce a new namespace without
/// touching the framework.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HookPointId(Cow<'static, str>);

impl HookPointId {
    /// Build a point id from a literal.
    pub const fn from_static(id: &'static str) -> Self {
        Self(Cow::Borrowed(id))
    }

    /// The subject's pre-inference point.
    pub const SUBJECT_BEFORE_INFERENCE: HookPointId =
        HookPointId::from_static("subject.before_inference");
    /// The subject's post-inference, pre-commit point.
    pub const SUBJECT_AFTER_INFERENCE: HookPointId =
        HookPointId::from_static("subject.after_inference");
    /// The subject's pre-commit effect point.
    pub const SUBJECT_BEFORE_OPERATION: HookPointId =
        HookPointId::from_static("subject.before_operation");
    /// The subject's post-commit point.
    pub const SUBJECT_AFTER_OPERATION: HookPointId =
        HookPointId::from_static("subject.after_operation");
    /// The runtime's capability change point.
    pub const RUNTIME_CAPABILITY_CHANGED: HookPointId =
        HookPointId::from_static("runtime.capability_changed");
    /// The runtime's generation handoff point.
    pub const RUNTIME_GENERATION_HANDOFF: HookPointId =
        HookPointId::from_static("runtime.generation_handoff");
    /// The context collection point of SPEC §41.8's stage table
    /// ("context collect"). A plugin that prepared material for this request
    /// contributes it here; the merged set is what the context policy
    /// assembles and seals.
    pub const CONTEXT_COLLECT: HookPointId = HookPointId::from_static("context.collect");

    /// Validate and own a point id.
    ///
    /// A point id must be `namespace.name` with two non-empty parts made of
    /// letters, digits, `_`, `-` and `.`. An unqualified name is refused so a
    /// plugin cannot claim a global stage.
    pub fn new(id: impl Into<String>) -> Result<Self, HookPointIdError> {
        let id = id.into();
        validate_point_id(&id)?;
        Ok(Self(Cow::Owned(id)))
    }

    /// The point id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The namespace part of the id.
    pub fn namespace(&self) -> &str {
        self.0.split_once('.').map(|(ns, _)| ns).unwrap_or("")
    }

    /// The local name part of the id.
    pub fn name(&self) -> &str {
        self.0.rsplit_once('.').map(|(_, name)| name).unwrap_or(&self.0)
    }
}

impl std::fmt::Debug for HookPointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for HookPointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for HookPointId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for HookPointId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        HookPointId::new(raw).map_err(serde::de::Error::custom)
    }
}

/// A malformed hook point id.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("hook point id {id:?} must be `namespace.name` with two non-empty parts")]
pub struct HookPointIdError {
    pub id: String,
}

fn validate_point_id(id: &str) -> Result<(), HookPointIdError> {
    let invalid = || HookPointIdError { id: id.to_owned() };
    let Some((namespace, name)) = id.split_once('.') else {
        return Err(invalid());
    };
    if namespace.is_empty() || name.is_empty() {
        return Err(invalid());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(invalid());
    }
    Ok(())
}

/// Synthesis contract of a hook point — SPEC §41.8.1 `mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookMode {
    /// Observes a decided result; cannot change it.
    Observe,
    /// Serial transformation of the point's value.
    Transform,
    /// Collection of independent contributions.
    Collect,
    /// A point-specific decision; the first valid decision ends the point.
    Decide,
    /// Middleware around the rest of the point; `next()` may be called at most
    /// once.
    Around,
}

impl HookMode {
    /// Every mode of the standard profile.
    pub const ALL: [HookMode; 5] = [
        HookMode::Observe,
        HookMode::Transform,
        HookMode::Collect,
        HookMode::Decide,
        HookMode::Around,
    ];
}

/// What dispatch does with a handler that fails, times out or panics —
/// SPEC §41.8.1 `failure`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookFailurePolicy {
    /// Skip this handler's output, keep the diagnostic, run the rest of the
    /// dispatch. The failure does not erase effects the handler already ran.
    ReportAndContinue,
    /// Stop the dispatch: keep the diagnostic and every result already
    /// collected, and run no further handler in this phase.
    StopUnit,
}

/// How a transform merges its replacement into the point's current value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransformMerge {
    /// The returned object is a patch: named fields are overwritten, other
    /// fields keep their previous value.
    #[default]
    PatchFields,
    /// The returned object replaces the whole value. Only meaningful when the
    /// point declares every field as writable.
    ReplaceWhole,
}

/// Declared contract of one hook point — SPEC §41.8.1 `HookPoint`.
///
/// The contract travels with the admitted graph: a snapshot keeps dispatching
/// under the contract it leased, and publishing a changed contract republishes
/// the whole graph through the same admission checks as a handler change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookPointContract {
    /// The point this contract describes.
    pub point: HookPointId,
    /// Contract major version. Handlers bind one `point_major`; admission
    /// refuses a handler compiled for a different major of this point.
    pub major: u32,
    /// How the outputs of this point are synthesized.
    pub mode: HookMode,
    /// Fields a transform handler may write. An empty list means no field may
    /// be written.
    #[serde(default)]
    pub writable_paths: Vec<String>,
    /// Slots a collect handler may write.
    #[serde(default)]
    pub writable_slots: Vec<ContributionSlot>,
    /// How a transform's replacement merges into the current value.
    #[serde(default)]
    pub transform_merge: TransformMerge,
    /// Finite per-handler budget. Zero is not a valid contract.
    pub timeout_ms: u64,
    pub failure: HookFailurePolicy,
    /// Schema of the point's input.
    pub input: SchemaRef,
    /// Schema of the point's output.
    pub output: SchemaRef,
}

impl HookPointContract {
    /// Standard install profile for one built-in point: observer contract,
    /// major 1, bounded 250ms per handler, report-and-continue.
    pub fn standard(point: HookPointId) -> Self {
        Self {
            point,
            major: STANDARD_HOOK_POINT_MAJOR,
            mode: HookMode::Observe,
            writable_paths: Vec::new(),
            writable_slots: Vec::new(),
            transform_merge: TransformMerge::PatchFields,
            timeout_ms: STANDARD_HOOK_TIMEOUT_MS,
            failure: HookFailurePolicy::ReportAndContinue,
            input: hook_schema("hook-input", HOOK_SCHEMA_VERSION),
            output: hook_schema("hook-output", HOOK_SCHEMA_VERSION),
        }
    }

    /// The standard profile of a `collect` point: contributions are merged
    /// by `(slot, key)` through the one shared rule, and a slot that is not
    /// declared writable is a write violation.
    pub fn collect(point: HookPointId, slots: Vec<ContributionSlot>) -> Self {
        Self {
            point,
            major: STANDARD_HOOK_POINT_MAJOR,
            mode: HookMode::Collect,
            writable_paths: Vec::new(),
            writable_slots: slots,
            transform_merge: TransformMerge::PatchFields,
            timeout_ms: STANDARD_HOOK_TIMEOUT_MS,
            failure: HookFailurePolicy::ReportAndContinue,
            input: hook_schema("hook-input", HOOK_SCHEMA_VERSION),
            output: hook_schema("hook-output", HOOK_SCHEMA_VERSION),
        }
    }

    /// True when a contribution slot may be written at this point.
    pub fn allows_slot(&self, slot: ContributionSlot) -> bool {
        self.writable_slots.contains(&slot)
    }

    /// True when a field path may be written at this point.
    pub fn allows_path(&self, path: &str) -> bool {
        self.writable_paths.iter().any(|allowed| allowed == path)
    }
}

fn hook_schema(name: &str, major: u32) -> SchemaRef {
    SchemaRef {
        name: name.to_owned(),
        major,
        digest: Digest::blake3(format!("{name}/v{major}").as_bytes()),
    }
}

/// The payload a point dispatches.
///
/// The built-in variants keep the subject's typed payloads; `Custom` carries
/// any point a plugin declared, so a new namespace needs no framework change.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "phase")]
pub enum HookPayload {
    BeforeInference {
        request_id: EntityId,
        agent_id: EntityId,
        model: String,
    },
    AfterInference {
        request_id: EntityId,
        stop_reason: String,
        output_tokens: u64,
    },
    BeforeOperation {
        operation_id: EntityId,
        operation_name: String,
    },
    AfterOperation {
        operation_id: EntityId,
        outcome: String,
        effect_status: String,
    },
    CapabilityChanged {
        capability_id: String,
        enabled: bool,
    },
    GenerationHandoff {
        old_generation: String,
        new_generation: String,
    },
    /// A point outside the built-in profile.
    Custom {
        point: HookPointId,
        value: serde_json::Value,
    },
}

impl HookPayload {
    /// The point this payload belongs to.
    pub fn point(&self) -> HookPointId {
        match self {
            HookPayload::BeforeInference { .. } => HookPointId::SUBJECT_BEFORE_INFERENCE,
            HookPayload::AfterInference { .. } => HookPointId::SUBJECT_AFTER_INFERENCE,
            HookPayload::BeforeOperation { .. } => HookPointId::SUBJECT_BEFORE_OPERATION,
            HookPayload::AfterOperation { .. } => HookPointId::SUBJECT_AFTER_OPERATION,
            HookPayload::CapabilityChanged { .. } => HookPointId::RUNTIME_CAPABILITY_CHANGED,
            HookPayload::GenerationHandoff { .. } => HookPointId::RUNTIME_GENERATION_HANDOFF,
            HookPayload::Custom { point, .. } => point.clone(),
        }
    }

    /// The value a transform point starts from.
    pub fn initial_value(&self) -> serde_json::Value {
        match self {
            HookPayload::Custom { value, .. } => value.clone(),
            other => serde_json::to_value(other).unwrap_or(serde_json::Value::Null),
        }
    }
}

/// What a handler returns — SPEC §41.8.1 `HookResult`.
///
/// The variant set is the point's mode contract; an output outside the
/// mode's legal set is never adopted, whatever the failure policy says.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum HookResult {
    /// Nothing changed.
    NoChange,
    /// A new value for a transform point.
    Replace { value: serde_json::Value },
    /// Contributions for a collect point.
    Contribute { items: Vec<ContextContribution> },
    /// A decision for a decide point.
    Decide { value: serde_json::Value },
    /// A follow-up task is needed; the source unit is not turned back into a
    /// wait.
    Defer { dependency_task_id: EntityId },
    /// An explicit refusal with a stable code.
    Reject { code: String, details: serde_json::Value },
}

impl HookResult {
    /// The legal outputs of a mode.
    pub fn is_legal_for(&self, mode: HookMode) -> bool {
        match mode {
            HookMode::Observe => matches!(self, HookResult::NoChange | HookResult::Defer { .. }),
            HookMode::Transform | HookMode::Around => matches!(
                self,
                HookResult::NoChange
                    | HookResult::Replace { .. }
                    | HookResult::Defer { .. }
                    | HookResult::Reject { .. }
            ),
            HookMode::Collect => matches!(
                self,
                HookResult::NoChange
                    | HookResult::Contribute { .. }
                    | HookResult::Defer { .. }
                    | HookResult::Reject { .. }
            ),
            HookMode::Decide => matches!(
                self,
                HookResult::NoChange
                    | HookResult::Decide { .. }
                    | HookResult::Defer { .. }
                    | HookResult::Reject { .. }
            ),
        }
    }
}

/// The receipt of one handler inside a dispatch.
///
/// `hook_id` and `implementation` are stamped by the dispatcher, so a retained
/// receipt names the exact implementation that ran.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookOutcome {
    pub hook_id: EntityId,
    /// What the handler returned.
    pub result: HookResult,
    /// Diagnostic for a failed or discarded handler.
    #[serde(default)]
    pub error: Option<String>,
    /// Artifact digest of the handler that produced this outcome.
    #[serde(default)]
    pub implementation: Option<Digest>,
    /// True when the output was adopted under the point's contract.
    pub adopted: bool,
    /// False only for an adopted explicit refusal.
    pub allowed: bool,
    /// True when the adopted output changed the point's value.
    pub modified: bool,
    /// The refusal reason, when the handler gave one.
    #[serde(default)]
    pub reason: Option<String>,
}

impl HookOutcome {
    fn failed(hook_id: EntityId, implementation: Option<Digest>, error: String) -> Self {
        Self {
            hook_id,
            result: HookResult::NoChange,
            error: Some(error),
            implementation,
            adopted: false,
            allowed: true,
            modified: false,
            reason: None,
        }
    }
}

/// Static description of one hook. It travels with the handler so the graph can
/// be validated and ordered before any handler runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSpec {
    pub hook_id: EntityId,
    pub plugin_id: EntityId,
    /// The point this handler subscribes to.
    pub point: HookPointId,
    pub name: String,
    pub schema_version: u32,
    /// Point contract major this handler was compiled against. Admission
    /// refuses a mismatch instead of reinterpreting the handler under a
    /// different contract.
    pub point_major: u32,
    /// Versioned identity of the handler artifact — SPEC §41.8.1
    /// `HookHandler.implementation`. It is part of the graph digest so a
    /// dispatch receipt can tell two implementations of the same hook id
    /// apart.
    pub implementation: VersionedRef,
    /// Optional ordering edges. Missing targets do not prevent activation.
    #[serde(default)]
    pub after: Vec<EntityId>,
    #[serde(default)]
    pub before: Vec<EntityId>,
    /// Required compatible handlers, independent of order. A required target
    /// may live in another point; it then asserts existence, not ordering.
    #[serde(default)]
    pub requires: Vec<EntityId>,
    /// True when the handler may issue a model call. It is a declaration for
    /// the resource accounting, not a permission.
    #[serde(default)]
    pub possible_model_call: bool,
    /// True when the handler's output is deterministic.
    #[serde(default)]
    pub deterministic: bool,
}

/// One admitted handler.
#[derive(Clone)]
pub struct HookEntry {
    pub spec: HookSpec,
    handler: HandlerKind,
}

#[derive(Clone)]
enum HandlerKind {
    Plain(Arc<dyn HookHandler>),
    Around(Arc<dyn AroundHookHandler>),
}

impl HookEntry {
    /// A plain handler.
    pub fn new(spec: HookSpec, handler: Arc<dyn HookHandler>) -> Self {
        Self {
            spec,
            handler: HandlerKind::Plain(handler),
        }
    }

    /// An around handler. Only an around point admits one.
    pub fn around(spec: HookSpec, handler: Arc<dyn AroundHookHandler>) -> Self {
        Self {
            spec,
            handler: HandlerKind::Around(handler),
        }
    }

    /// The implementation artifact digest of this handler.
    pub fn implementation_digest(&self) -> Digest {
        self.spec.implementation.artifact.digest.clone()
    }
}

/// A handler that observes or produces an output for one dispatch.
#[async_trait]
pub trait HookHandler: Send + Sync + 'static {
    async fn handle(&self, payload: &HookPayload) -> anyhow::Result<HookResult>;
}

/// A handler that wraps the rest of its point.
///
/// The handler may call `next()` at most once. The type system enforces it:
/// `AroundNext` is consumed by the call and cannot be cloned.
#[async_trait]
pub trait AroundHookHandler: Send + Sync + 'static {
    async fn handle(
        &self,
        payload: &HookPayload,
        next: AroundNext<'_>,
    ) -> anyhow::Result<HookResult>;
}

/// The continuation of an around handler.
///
/// Calling it runs the rest of the point exactly once.
pub struct AroundNext<'a> {
    inner: Option<Pin<Box<dyn Future<Output = AroundOutcome> + Send + 'a>>>,
}

impl<'a> AroundNext<'a> {
    fn new(inner: Pin<Box<dyn Future<Output = AroundOutcome> + Send + 'a>>) -> Self {
        Self { inner: Some(inner) }
    }

    /// Run the rest of the point.
    pub async fn call(mut self) -> AroundOutcome {
        match self.inner.take() {
            Some(future) => future.await,
            None => AroundOutcome::failed("around handler called next() more than once"),
        }
    }
}

/// What the rest of an around point produced.
#[derive(Debug, Clone, Default)]
pub struct AroundOutcome {
    /// Outcomes of the handlers after the around handler.
    pub results: Vec<HookOutcome>,
    /// Contributions the inner chain collected.
    pub contributions: Vec<ContextContribution>,
    /// The value the inner chain produced.
    pub value: Option<serde_json::Value>,
    /// The decision the inner chain produced.
    pub decision: Option<serde_json::Value>,
    /// True when the inner chain stopped early.
    pub stopped_early: bool,
    /// Diagnostic of a refused continuation.
    pub error: Option<String>,
}

impl AroundOutcome {
    fn failed(error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
            ..Default::default()
        }
    }
}

/// Identity of one published registration, not merely its reusable hook id.
/// Keeping this handle does not keep a retired handler alive; withdrawal is
/// explicit and stale handles cannot remove a later registration.
#[derive(Clone, Debug)]
pub struct HookRegistration {
    hook_id: EntityId,
    entry: Weak<HookEntry>,
}

impl HookRegistration {
    pub fn hook_id(&self) -> EntityId {
        self.hook_id
    }
}

/// Receipt for one whole-graph publication and its independently owned entries.
#[derive(Debug)]
pub struct HookPublication {
    pub generation: u64,
    pub registrations: Vec<HookRegistration>,
}

/// Receipt of one point dispatch — SPEC §41.8.1 `HookDispatch`.
///
/// It records the durable identity of what ran: the leased graph revision and
/// its digest, plus every handler result stamped with the artifact that
/// produced it. `stopped_early` means a `stop_unit` contract cut the dispatch
/// short; the failing result carries the diagnostic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookDispatch {
    pub dispatch_id: EntityId,
    /// The point that ran.
    pub point: HookPointId,
    /// Runtime-only revision of the leased graph.
    pub generation: u64,
    /// Content identity of the leased graph: contracts plus member specs and
    /// implementations. Two graphs sharing a digest dispatch identically, even
    /// across restarts.
    pub graph_digest: Digest,
    /// Implementation artifact digest of every ordered handler of the leased
    /// graph — SPEC §41.8.1 `handler_digests`.
    #[serde(default)]
    pub handler_digests: Vec<Digest>,
    /// The input revision this dispatch ran against — SPEC §41.8.1
    /// `input_revision`. A changed input creates a new dispatch instead of
    /// appending a stale asynchronous result.
    #[serde(default)]
    pub input_revision: String,
    /// The sealed input this dispatch ran against, when the caller has one.
    #[serde(default)]
    pub input: Option<DataRef>,
    /// The request this dispatch belongs to, when it is a request dispatch.
    #[serde(default)]
    pub request_id: Option<EntityId>,
    /// The revision of that request.
    #[serde(default)]
    pub request_revision: Option<u64>,
    /// Per-handler outcomes.
    pub results: Vec<HookOutcome>,
    pub stopped_early: bool,
    /// False when the point was never declared in the leased graph.
    #[serde(default)]
    pub point_declared: bool,
    /// Contributions merged by a collect point.
    #[serde(default)]
    pub contributions: Vec<ContextContribution>,
    /// The value produced by a transform point.
    #[serde(default)]
    pub value: Option<serde_json::Value>,
    /// The decision produced by a decide point.
    #[serde(default)]
    pub decision: Option<serde_json::Value>,
}

impl HookDispatch {
    /// True when the dispatch refused the unit.
    pub fn allows(&self) -> bool {
        !self.stopped_early && self.results.iter().all(|outcome| outcome.allowed)
    }
}

/// What the caller knows about the dispatch it starts.
///
/// The kernel does not invent these facts: the caller owns the input
/// revision, the sealed input and the request identity, and the receipt
/// carries exactly what the caller supplied.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DispatchContext {
    /// The revision of the input this dispatch runs against.
    #[serde(default)]
    pub input_revision: String,
    /// The sealed input object this dispatch runs against.
    #[serde(default)]
    pub input: Option<DataRef>,
    /// The request this dispatch belongs to.
    #[serde(default)]
    pub request_id: Option<EntityId>,
    /// The revision of that request.
    #[serde(default)]
    pub request_revision: Option<u64>,
}

/// A refusal of a candidate graph. The running graph stays in force.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum HookAdmissionError {
    #[error("hook {hook_id} declares schema {found}; this registry admits {expected}")]
    UnknownSchema {
        hook_id: EntityId,
        expected: u32,
        found: u32,
    },
    #[error("hook {0} is already registered")]
    DuplicateHook(EntityId),
    #[error("plugin {plugin_id} already owns hook name {name} in {point}")]
    DuplicateName {
        plugin_id: EntityId,
        point: HookPointId,
        name: String,
    },
    #[error("hook {hook_id} depends on {missing}, which is not an admitted hook")]
    MissingDependency {
        hook_id: EntityId,
        missing: EntityId,
    },
    #[error("hook {hook_id} depends on {dependency}, which belongs to another point")]
    CrossPointDependency {
        hook_id: EntityId,
        dependency: EntityId,
    },
    #[error("hook graph has a cycle; blocked handlers: {cycle:?}")]
    Cycle { cycle: Vec<EntityId> },
    #[error("hook graph revision exhausted")]
    RevisionExhausted,
    #[error("hook graph changed during candidate preparation: expected {expected}, found {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("hook {hook_id} was compiled for {point} major {handler_major}; the admitted contract is major {point_major}")]
    PointMajorMismatch {
        hook_id: EntityId,
        point: HookPointId,
        point_major: u32,
        handler_major: u32,
    },
    #[error("hook point {point} contract must give a finite timeout_ms > 0")]
    InvalidPointContract { point: HookPointId },
    #[error("hook {hook_id} subscribes to undeclared point {point}")]
    UndeclaredPoint {
        hook_id: EntityId,
        point: HookPointId,
    },
    #[error("point {point} mode {mode:?} does not admit an around handler")]
    AroundHandlerInWrongMode {
        point: HookPointId,
        mode: HookMode,
    },
    #[error("point {point} mode {mode:?} does not admit a plain handler")]
    PlainHandlerInWrongMode {
        point: HookPointId,
        mode: HookMode,
    },
}

/// The admitted graph. Replaced only as a whole, so a rejected registration
/// never leaves a half-applied graph behind.
struct Graph {
    generation: u64,
    /// Every declared point contract, including points that have no member
    /// yet. A point is declared before its handlers exist, so the contract
    /// survives a graph rebuild that has no member for it.
    contracts: BTreeMap<HookPointId, HookPointContract>,
    /// The ordered members of each point.
    points: BTreeMap<HookPointId, Vec<Arc<HookEntry>>>,
    /// Content identity of this graph: digest over contracts plus every
    /// member spec including its versioned implementation. Deterministic for
    /// identical declarations, independent of registration order or uptime.
    digest: Digest,
}

impl Graph {
    fn standard_contracts() -> BTreeMap<HookPointId, HookPointContract> {
        let mut contracts: BTreeMap<HookPointId, HookPointContract> = [
            HookPointId::SUBJECT_BEFORE_INFERENCE,
            HookPointId::SUBJECT_AFTER_INFERENCE,
            HookPointId::SUBJECT_BEFORE_OPERATION,
            HookPointId::SUBJECT_AFTER_OPERATION,
            HookPointId::RUNTIME_CAPABILITY_CHANGED,
            HookPointId::RUNTIME_GENERATION_HANDOFF,
        ]
        .into_iter()
        .map(|point| (point.clone(), HookPointContract::standard(point)))
        .collect();
        // The context collection point of SPEC §41.8's stage table. A plugin
        // may contribute into every slot except the system contract, which
        // the kernel owns.
        contracts.insert(
            HookPointId::CONTEXT_COLLECT.clone(),
            HookPointContract::collect(
                HookPointId::CONTEXT_COLLECT.clone(),
                ContributionSlot::ALL
                    .into_iter()
                    .filter(|slot| slot.accepts_transient())
                    .collect(),
            ),
        );
        contracts
    }

    fn compute_digest(
        contracts: &BTreeMap<HookPointId, HookPointContract>,
        members: &[&HookSpec],
    ) -> Digest {
        #[derive(Serialize)]
        struct GraphIdentity<'a> {
            schema_version: u32,
            contracts: &'a BTreeMap<HookPointId, HookPointContract>,
            members: &'a [&'a HookSpec],
        }
        let identity = GraphIdentity {
            schema_version: HOOK_SCHEMA_VERSION,
            contracts,
            members,
        };
        let bytes = serde_json::to_vec(&identity).expect("hook graph identity is serializable");
        Digest::blake3(&bytes)
    }

    /// Rebuild the graph from a candidate set of entries and contracts.
    fn build(
        generation: u64,
        entries: Vec<Arc<HookEntry>>,
        contracts: BTreeMap<HookPointId, HookPointContract>,
    ) -> Result<Self, HookAdmissionError> {
        let mut by_id: BTreeMap<EntityId, usize> = BTreeMap::new();
        let mut names: BTreeSet<(EntityId, HookPointId, String)> = BTreeSet::new();
        for (index, entry) in entries.iter().enumerate() {
            if entry.spec.schema_version != HOOK_SCHEMA_VERSION {
                return Err(HookAdmissionError::UnknownSchema {
                    hook_id: entry.spec.hook_id,
                    expected: HOOK_SCHEMA_VERSION,
                    found: entry.spec.schema_version,
                });
            }
            let Some(contract) = contracts.get(&entry.spec.point) else {
                return Err(HookAdmissionError::UndeclaredPoint {
                    hook_id: entry.spec.hook_id,
                    point: entry.spec.point.clone(),
                });
            };
            if entry.spec.point_major != contract.major {
                return Err(HookAdmissionError::PointMajorMismatch {
                    hook_id: entry.spec.hook_id,
                    point: entry.spec.point.clone(),
                    point_major: contract.major,
                    handler_major: entry.spec.point_major,
                });
            }
            match (&entry.handler, contract.mode) {
                (HandlerKind::Around(_), HookMode::Around) => {}
                (HandlerKind::Around(_), mode) => {
                    return Err(HookAdmissionError::AroundHandlerInWrongMode {
                        point: entry.spec.point.clone(),
                        mode,
                    });
                }
                (HandlerKind::Plain(_), HookMode::Around) => {
                    return Err(HookAdmissionError::PlainHandlerInWrongMode {
                        point: entry.spec.point.clone(),
                        mode: contract.mode,
                    });
                }
                (HandlerKind::Plain(_), _) => {}
            }
            if !names.insert((
                entry.spec.plugin_id,
                entry.spec.point.clone(),
                entry.spec.name.clone(),
            )) {
                return Err(HookAdmissionError::DuplicateName {
                    plugin_id: entry.spec.plugin_id,
                    point: entry.spec.point.clone(),
                    name: entry.spec.name.clone(),
                });
            }
            if by_id.insert(entry.spec.hook_id, index).is_some() {
                return Err(HookAdmissionError::DuplicateHook(entry.spec.hook_id));
            }
        }

        // A `before`/`after` edge only constrains handlers of the same point.
        // A `requires` edge may name a handler of another point; it then only
        // asserts that the dependency exists and was admitted.
        for entry in entries.iter() {
            for dependency in entry
                .spec
                .after
                .iter()
                .chain(&entry.spec.before)
                .chain(&entry.spec.requires)
            {
                let Some(&other) = by_id.get(dependency) else {
                    if entry.spec.requires.contains(dependency) {
                        return Err(HookAdmissionError::MissingDependency {
                            hook_id: entry.spec.hook_id,
                            missing: *dependency,
                        });
                    }
                    continue;
                };
                if entry.spec.requires.contains(dependency) {
                    continue;
                }
                if entries[other].spec.point != entry.spec.point {
                    return Err(HookAdmissionError::CrossPointDependency {
                        hook_id: entry.spec.hook_id,
                        dependency: *dependency,
                    });
                }
            }
        }

        // Order each point independently: a topological sort of the declared
        // edges with the stable hook id as the tie break.
        let mut points: BTreeMap<HookPointId, Vec<Arc<HookEntry>>> = BTreeMap::new();
        for point in contracts.keys() {
            let members: Vec<Arc<HookEntry>> = entries
                .iter()
                .filter(|entry| &entry.spec.point == point)
                .cloned()
                .collect();
            if members.is_empty() {
                continue;
            }
            points.insert(point.clone(), order_point(&members)?);
        }
        let ordered_specs: Vec<&HookSpec> = points
            .values()
            .flat_map(|members| members.iter().map(|entry| &entry.spec))
            .collect();
        let digest = Self::compute_digest(&contracts, &ordered_specs);
        Ok(Self {
            generation,
            contracts,
            points,
            digest,
        })
    }

    /// The ordered members of one point.
    fn members(&self, point: &HookPointId) -> Option<&[Arc<HookEntry>]> {
        self.points.get(point).map(|members| members.as_slice())
    }

    /// The contract of one point.
    fn contract(&self, point: &HookPointId) -> Option<&HookPointContract> {
        self.contracts.get(point)
    }

    /// The binding of this graph.
    fn binding(&self, lease_epoch: u64) -> HookGraphBinding {
        let mut point_ids = Vec::new();
        let mut handler_digests = Vec::new();
        for (point, members) in &self.points {
            point_ids.push(point.as_str().to_owned());
            for entry in members {
                handler_digests.push(entry.implementation_digest());
            }
        }
        HookGraphBinding {
            graph_digest: self.digest.clone(),
            revision: self.generation,
            point_ids,
            handler_digests,
            lease_epoch,
        }
    }

    /// The implementation artifact digest of every ordered handler, in
    /// dispatch order — SPEC §41.8.1 `handler_digests`.
    fn handler_digests(&self) -> Vec<Digest> {
        self.points
            .values()
            .flat_map(|members| members.iter().map(|entry| entry.implementation_digest()))
            .collect()
    }

    /// Every member of the graph, in point order.
    fn entries(&self) -> Vec<Arc<HookEntry>> {
        self.points
            .values()
            .flat_map(|members| members.iter().cloned())
            .collect()
    }

    fn contains(&self, entry: &Arc<HookEntry>) -> bool {
        self.points
            .values()
            .flat_map(|members| members.iter())
            .any(|candidate| Arc::ptr_eq(candidate, entry))
    }
}

/// Topological order of one point's members.
fn order_point(members: &[Arc<HookEntry>]) -> Result<Vec<Arc<HookEntry>>, HookAdmissionError> {
    let mut by_id: BTreeMap<EntityId, usize> = BTreeMap::new();
    for (index, entry) in members.iter().enumerate() {
        by_id.insert(entry.spec.hook_id, index);
    }
    let mut edges: BTreeSet<(usize, usize)> = BTreeSet::new();
    let mut successors: Vec<Vec<usize>> = vec![Vec::new(); members.len()];
    let mut indegrees: Vec<usize> = vec![0; members.len()];
    for (current, entry) in members.iter().enumerate() {
        for dependency in &entry.spec.after {
            if let Some(&predecessor) = by_id.get(dependency) {
                edges.insert((predecessor, current));
            }
        }
        for dependency in &entry.spec.before {
            if let Some(&successor) = by_id.get(dependency) {
                edges.insert((current, successor));
            }
        }
    }
    for (from, to) in edges {
        successors[from].push(to);
        indegrees[to] += 1;
    }
    let mut ready: BTreeSet<(EntityId, usize)> = members
        .iter()
        .enumerate()
        .filter(|(index, _)| indegrees[*index] == 0)
        .map(|(index, entry)| (entry.spec.hook_id, index))
        .collect();
    let mut order = Vec::with_capacity(members.len());
    while let Some((_, index)) = ready.pop_first() {
        order.push(index);
        for &successor in &successors[index] {
            indegrees[successor] -= 1;
            if indegrees[successor] == 0 {
                ready.insert((members[successor].spec.hook_id, successor));
            }
        }
    }
    if order.len() != members.len() {
        let cycle = members
            .iter()
            .enumerate()
            .filter(|(index, _)| indegrees[*index] > 0)
            .map(|(_, entry)| entry.spec.hook_id)
            .collect();
        return Err(HookAdmissionError::Cycle { cycle });
    }
    Ok(order
        .into_iter()
        .map(|index| Arc::clone(&members[index]))
        .collect())
}

/// Live lease accounting, so a teardown can wait for the real barrier.
#[derive(Default)]
struct LeaseState {
    /// Every lease ever issued. It is the lease epoch a request seals.
    issued: AtomicU64,
    counts: Mutex<BTreeMap<u64, usize>>,
    notify: tokio::sync::Notify,
}

/// The bound of the pending-event queue. A runtime that changes faster than
/// the graph is drained keeps the newest events; the dropped count is
/// reported instead of pretending every change was observed.
const MAX_PENDING_EVENTS: usize = 1024;

/// Events the runtime produced but the graph has not dispatched yet.
#[derive(Default)]
struct PendingEvents {
    queue: VecDeque<HookPayload>,
    dropped: u64,
}

/// The guard of one leased graph.
struct LeaseGuard {
    state: Arc<LeaseState>,
    generation: u64,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        let mut counts = self.state.counts.lock();
        if let Some(count) = counts.get_mut(&self.generation) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&self.generation);
            }
        }
        drop(counts);
        self.state.notify.notify_waiters();
    }
}

/// The hook registry: the admitted graph and its live leases.
pub struct HookRegistry {
    graph: RwLock<Arc<Graph>>,
    leases: Arc<LeaseState>,
    pending: Mutex<PendingEvents>,
    errors: Arc<crate::errors::ErrorObservation>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::new_with_errors(Arc::new(crate::errors::ErrorObservation::new()))
    }

    /// Share the assembly's single observation point so one kernel incident
    /// is one journal entry.
    pub fn new_with_errors(errors: Arc<crate::errors::ErrorObservation>) -> Self {
        let contracts = Graph::standard_contracts();
        let digest = Graph::compute_digest(&contracts, &[]);
        Self {
            graph: RwLock::new(Arc::new(Graph {
                generation: 0,
                contracts,
                points: BTreeMap::new(),
                digest,
            })),
            leases: Arc::new(LeaseState::default()),
            pending: Mutex::new(PendingEvents::default()),
            errors,
        }
    }

    pub fn generation(&self) -> u64 {
        self.graph.read().generation
    }

    /// Admit one hook. The current graph stays in force when the addition is
    /// rejected for any reason.
    pub fn register(&self, entry: HookEntry) -> Result<HookRegistration, HookAdmissionError> {
        let graph = Arc::clone(&self.graph.read());
        let entry = Arc::new(entry);
        let registration = HookRegistration {
            hook_id: entry.spec.hook_id,
            entry: Arc::downgrade(&entry),
        };
        let mut entries = graph.entries();
        entries.push(entry);
        let revision = graph
            .generation
            .checked_add(1)
            .ok_or(HookAdmissionError::RevisionExhausted)?;
        let rebuilt = Graph::build(revision, entries, graph.contracts.clone())?;
        self.publish(graph.generation, rebuilt)?;
        Ok(registration)
    }

    /// Withdraw exactly the supplied registration. A stale or foreign handle
    /// is a no-op and does not advance the graph revision. Removing a required
    /// dependency is still rejected by the existing candidate validator.
    pub fn unregister(&self, registration: &HookRegistration) -> Result<bool, HookAdmissionError> {
        let graph = Arc::clone(&self.graph.read());
        let Some(owned) = registration.entry.upgrade() else {
            return Ok(false);
        };
        if !graph.contains(&owned) {
            return Ok(false);
        }
        let entries: Vec<Arc<HookEntry>> = graph
            .entries()
            .into_iter()
            .filter(|entry| !Arc::ptr_eq(entry, &owned))
            .collect();
        // Rebuilding rejects a removal that would break a declared dependency,
        // and the previous graph stays in force in that case.
        let revision = graph
            .generation
            .checked_add(1)
            .ok_or(HookAdmissionError::RevisionExhausted)?;
        let rebuilt = Graph::build(revision, entries, graph.contracts.clone())?;
        self.publish(graph.generation, rebuilt)?;
        Ok(true)
    }

    /// Admit or replace the declared contract of one hook point.
    ///
    /// The whole graph is re-validated and republished: a major-version bump
    /// fences handlers still declaring the old major (their candidates are
    /// refused with `PointMajorMismatch`), while in-flight snapshots keep
    /// dispatching under the contract they leased. Publishing the identical
    /// contract is a no-op that keeps the current revision.
    pub fn declare_point(&self, contract: HookPointContract) -> Result<(), HookAdmissionError> {
        if contract.timeout_ms == 0 {
            return Err(HookAdmissionError::InvalidPointContract {
                point: contract.point.clone(),
            });
        }
        let graph = Arc::clone(&self.graph.read());
        if graph.contract(&contract.point) == Some(&contract) {
            return Ok(());
        }
        let mut contracts = graph.contracts.clone();
        contracts.insert(contract.point.clone(), contract);
        let generation = graph
            .generation
            .checked_add(1)
            .ok_or(HookAdmissionError::RevisionExhausted)?;
        let rebuilt = Graph::build(generation, graph.entries(), contracts)?;
        self.publish(graph.generation, rebuilt)
            .map_err(|error| self.observe_admission_refusal("declare_point", error))?;
        Ok(())
    }

    /// Replace the whole graph for a new plugin generation. Validation happens
    /// before the swap, and a run that already snapshotted the previous graph
    /// keeps using it until that run finishes.
    pub fn swap_generation(
        &self,
        entries: Vec<HookEntry>,
    ) -> Result<HookPublication, HookAdmissionError> {
        let graph = Arc::clone(&self.graph.read());
        let generation = graph
            .generation
            .checked_add(1)
            .ok_or(HookAdmissionError::RevisionExhausted)?;
        let built = Graph::build(
            generation,
            entries.into_iter().map(Arc::new).collect(),
            graph.contracts.clone(),
        )
        .map_err(|error| self.observe_admission_refusal("swap_generation", error));
        let rebuilt = built?;
        let registrations = rebuilt
            .points
            .values()
            .flat_map(|members| members.iter())
            .map(|entry| HookRegistration {
                hook_id: entry.spec.hook_id,
                entry: Arc::downgrade(entry),
            })
            .collect();
        self.publish(graph.generation, rebuilt)
            .map_err(|error| self.observe_admission_refusal("swap_generation", error))?;
        Ok(HookPublication {
            generation,
            registrations,
        })
    }

    fn publish(&self, expected: u64, candidate: Graph) -> Result<(), HookAdmissionError> {
        let mut graph = self.graph.write();
        if graph.generation != expected {
            // A concurrent mutation raced the publish: the runtime's own
            // revision discipline was violated, so this is a defect entry,
            // not a caller-bug refusal.
            self.errors.observe(
                crate::errors::ErrorSite::new("hook_graph_revision_conflict", module_path!()),
                format!(
                    "hook graph publish revision conflict: expected generation {expected}, actual {}",
                    graph.generation
                ),
                serde_json::json!({ "expected": expected, "actual": graph.generation }),
            );
            return Err(HookAdmissionError::RevisionConflict {
                expected,
                actual: graph.generation,
            });
        }
        let previous = std::mem::replace(&mut *graph, Arc::new(candidate));
        drop(graph);
        drop(previous);
        Ok(())
    }

    /// A refused candidate graph is the dynamic-graph downgrade boundary:
    /// the previous graph stays in force, and nothing else records that the
    /// runtime degraded. One observation per refusal; the caller still
    /// receives the same error.
    fn observe_admission_refusal(
        &self,
        boundary: &'static str,
        error: HookAdmissionError,
    ) -> HookAdmissionError {
        // `publish` already journal-ed its own RevisionConflict as a defect;
        // do not record the same incident twice under two kinds.
        if matches!(error, HookAdmissionError::RevisionConflict { .. }) {
            return error;
        }
        self.errors.observe(
            crate::errors::ErrorSite::new("hook_graph_admission", module_path!()),
            format!("hook graph admission refused at {boundary}: {error}"),
            serde_json::json!({ "boundary": boundary }),
        );
        error
    }

    /// Every admitted handler with its point, in dispatch order.
    pub fn list(&self) -> Vec<(String, HookPointId)> {
        let graph = self.graph.read();
        graph
            .points
            .iter()
            .flat_map(|(point, members)| {
                members
                    .iter()
                    .map(move |entry| (entry.spec.name.clone(), point.clone()))
            })
            .collect()
    }

    /// Ordered hook ids for one point, as the registry will run them.
    pub fn order(&self, point: &HookPointId) -> Vec<EntityId> {
        self.graph
            .read()
            .members(point)
            .map(|members| members.iter().map(|entry| entry.spec.hook_id).collect())
            .unwrap_or_default()
    }

    /// The declared contracts of the admitted graph.
    pub fn contracts(&self) -> BTreeMap<HookPointId, HookPointContract> {
        self.graph.read().contracts.clone()
    }

    /// Lease one immutable graph for a whole invocation, including its later
    /// points. The lease retains the old handlers through an intervening swap
    /// and is counted, so a teardown can wait for the real barrier.
    pub fn snapshot(&self) -> HookSnapshot {
        let graph = Arc::clone(&self.graph.read());
        let epoch = self.leases.issued.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let mut counts = self.leases.counts.lock();
            *counts.entry(graph.generation).or_insert(0) += 1;
        }
        let lease = Arc::new(LeaseGuard {
            state: Arc::clone(&self.leases),
            generation: graph.generation,
        });
        HookSnapshot {
            graph,
            lease,
            epoch,
        }
    }

    /// Durable content identity of the currently admitted graph.
    pub fn graph_digest(&self) -> Digest {
        self.graph.read().digest.clone()
    }

    /// The binding of the currently admitted graph.
    pub fn binding(&self) -> HookGraphBinding {
        let epoch = self.leases.issued.load(Ordering::SeqCst);
        let graph = self.graph.read();
        graph.binding(epoch)
    }

    /// The number of live leases of one revision.
    pub fn live_leases(&self, revision: u64) -> usize {
        self.leases.counts.lock().get(&revision).copied().unwrap_or(0)
    }

    /// Wait until no lease of `revision` is live.
    ///
    /// This is the real barrier a teardown waits for; it never guesses that an
    /// in-flight dispatch has already finished.
    pub async fn await_quiescence(&self, revision: u64) -> bool {
        loop {
            let notified = self.leases.notify.notified();
            if self.live_leases(revision) == 0 {
                return true;
            }
            notified.await;
        }
    }

    /// Dispatch one point under the current graph.
    pub async fn dispatch(&self, payload: &HookPayload) -> HookDispatch {
        self.dispatch_at(payload, DispatchContext::default()).await
    }

    /// Dispatch one point under the current graph, recording what the caller
    /// knows about the dispatch (input revision, sealed input, request).
    pub async fn dispatch_at(
        &self,
        payload: &HookPayload,
        context: DispatchContext,
    ) -> HookDispatch {
        self.snapshot().dispatch_at(payload, context).await
    }

    /// Keep the existing observer policy: only an explicit adopted refusal
    /// disallows. A dispatch cut short by a `stop_unit` contract does not prove
    /// the unit may proceed, so it reads as not allowed.
    pub async fn check_allowed(&self, payload: &HookPayload) -> bool {
        self.dispatch(payload).await.allows()
    }

    /// Enqueue one runtime event for the next drain.
    ///
    /// The runtime registry produces events (a capability appeared, a
    /// provider was withdrawn) without owning an async runtime, so the event
    /// is queued here and dispatched at the next defined boundary. The queue
    /// is bounded: an undrained burst keeps the newest events and counts what
    /// it dropped, instead of growing without limit.
    pub fn enqueue(&self, payload: HookPayload) {
        let mut pending = self.pending.lock();
        if pending.queue.len() >= MAX_PENDING_EVENTS {
            pending.queue.pop_front();
            pending.dropped += 1;
        }
        pending.queue.push_back(payload);
    }

    /// The number of queued events.
    pub fn pending_len(&self) -> usize {
        self.pending.lock().queue.len()
    }

    /// How many events were dropped because the queue was full.
    pub fn dropped_events(&self) -> u64 {
        self.pending.lock().dropped
    }

    /// Dispatch every queued event under one leased graph.
    ///
    /// All events of one drain share a single lease, so the receipts name one
    /// graph revision and one digest. The queue is emptied before the
    /// dispatch, so a handler that produces a new event queues it for the
    /// next drain instead of extending this one.
    pub async fn drain_pending(&self) -> Vec<HookDispatch> {
        let events: Vec<HookPayload> = {
            let mut pending = self.pending.lock();
            pending.queue.drain(..).collect()
        };
        if events.is_empty() {
            return Vec::new();
        }
        let snapshot = self.snapshot();
        let mut dispatches = Vec::with_capacity(events.len());
        for event in events {
            dispatches.push(snapshot.dispatch(&event).await);
        }
        dispatches
    }

    /// Wait until every lease of the currently admitted graph is released.
    ///
    /// This is the real barrier a host teardown waits for: it never assumes
    /// that an in-flight dispatch has already finished.
    pub async fn await_current_quiescence(&self) -> bool {
        let revision = self.graph.read().generation;
        self.await_quiescence(revision).await
    }
}

/// A leased graph. Every point dispatched through it uses the same contracts
/// and the same handler artifacts.
#[derive(Clone)]
pub struct HookSnapshot {
    graph: Arc<Graph>,
    lease: Arc<LeaseGuard>,
    epoch: u64,
}

impl HookSnapshot {
    pub fn generation(&self) -> u64 {
        self.graph.generation
    }

    /// Durable content identity of the leased graph, including its admitted
    /// point contracts and member implementations.
    pub fn graph_digest(&self) -> &Digest {
        &self.graph.digest
    }

    /// The binding of the leased graph — the identity a request seals.
    pub fn binding(&self) -> HookGraphBinding {
        self.graph.binding(self.epoch)
    }

    /// Dispatch one point under the leased contract set.
    ///
    /// Each handler gets the point's finite `timeout_ms`. A timeout, error or
    /// panic becomes a diagnostic outcome and is handled by the point's
    /// failure policy: `report_and_continue` skips the output and runs the
    /// rest of the dispatch; `stop_unit` stops the dispatch after recording
    /// it. Illegal verdicts from the point's mode are never adopted, whatever
    /// the failure policy says.
    pub async fn dispatch(&self, payload: &HookPayload) -> HookDispatch {
        self.dispatch_at(payload, DispatchContext::default()).await
    }

    /// Dispatch one point under the leased contract set, recording what the
    /// caller knows about the dispatch.
    pub async fn dispatch_at(
        &self,
        payload: &HookPayload,
        context: DispatchContext,
    ) -> HookDispatch {
        let point = payload.point();
        let Some(contract) = self.graph.contract(&point).cloned() else {
            return HookDispatch {
                dispatch_id: EntityId::new(),
                point,
                generation: self.graph.generation,
                graph_digest: self.graph.digest.clone(),
                handler_digests: self.graph.handler_digests(),
                input_revision: context.input_revision,
                input: context.input,
                request_id: context.request_id,
                request_revision: context.request_revision,
                results: Vec::new(),
                stopped_early: false,
                point_declared: false,
                contributions: Vec::new(),
                value: None,
                decision: None,
            };
        };
        let members = self
            .graph
            .members(&point)
            .map(|members| members.to_vec())
            .unwrap_or_default();
        let state = Arc::new(Mutex::new(DispatchState::default()));
        let chain = Chain {
            members,
            contract,
            payload: payload.clone(),
            state: Arc::clone(&state),
        };
        run_chain(&chain, 0).await;
        let state = state.lock();
        HookDispatch {
            dispatch_id: EntityId::new(),
            point,
            generation: self.graph.generation,
            graph_digest: self.graph.digest.clone(),
            handler_digests: self.graph.handler_digests(),
            input_revision: context.input_revision,
            input: context.input,
            request_id: context.request_id,
            request_revision: context.request_revision,
            results: state.results.clone(),
            stopped_early: state.stopped_early,
            point_declared: true,
            contributions: state.contributions.clone(),
            value: state.value.clone(),
            decision: state.decision.clone(),
        }
    }
}

/// The mutable state of one dispatch.
#[derive(Default)]
struct DispatchState {
    results: Vec<HookOutcome>,
    contributions: Vec<ContextContribution>,
    value: Option<serde_json::Value>,
    decision: Option<serde_json::Value>,
    stopped_early: bool,
}

/// Everything one chain of handlers needs.
struct Chain {
    members: Vec<Arc<HookEntry>>,
    contract: HookPointContract,
    payload: HookPayload,
    state: Arc<Mutex<DispatchState>>,
}

/// Run the handlers of one point from `index` onwards.
async fn run_chain(chain: &Chain, mut index: usize) -> AroundOutcome {
    let mut local = AroundOutcome::default();
    while index < chain.members.len() {
        if chain.state.lock().stopped_early {
            local.stopped_early = true;
            break;
        }
        let entry = Arc::clone(&chain.members[index]);
        let is_around = matches!(entry.handler, HandlerKind::Around(_));
        let outcome = match &entry.handler {
            HandlerKind::Plain(handler) => run_plain(chain, &entry, handler.as_ref()).await,
            HandlerKind::Around(handler) => {
                let next = AroundNext::new(run_chain_continuation(chain, index + 1));
                run_around(chain, &entry, handler.as_ref(), next).await
            }
        };
        match outcome {
            Err(diagnostic) => {
                // A handler failure is governed by the point's failure policy.
                tracing::warn!(
                    hook = %entry.spec.hook_id,
                    err = %diagnostic,
                    "hook handler failed; output skipped"
                );
                let failed = HookOutcome::failed(
                    entry.spec.hook_id,
                    Some(entry.implementation_digest()),
                    diagnostic,
                );
                chain.state.lock().results.push(failed.clone());
                local.results.push(failed);
                if chain.contract.failure == HookFailurePolicy::StopUnit {
                    chain.state.lock().stopped_early = true;
                    local.stopped_early = true;
                }
                break;
            }
            Ok(outcome) => {
                let adopted = outcome.adopted;
                chain.state.lock().results.push(outcome.clone());
                local.results.push(outcome);
                if adopted && chain.state.lock().decision.is_some() {
                    // The first valid decision ends this point.
                    break;
                }
            }
        }
        if is_around {
            // The around handler owns the rest of the point: it either ran
            // the continuation, which already executed the remaining
            // handlers, or it deliberately skipped them.
            break;
        }
        index += 1;
    }
    let state = chain.state.lock();
    local.contributions = state.contributions.clone();
    local.value = state.value.clone();
    local.decision = state.decision.clone();
    local
}

fn run_chain_continuation(
    chain: &Chain,
    index: usize,
) -> Pin<Box<dyn Future<Output = AroundOutcome> + Send + '_>> {
    Box::pin(run_chain(chain, index))
}

/// Run one plain handler under the point's budget.
async fn run_plain(
    chain: &Chain,
    entry: &Arc<HookEntry>,
    handler: &dyn HookHandler,
) -> Result<HookOutcome, String> {
    let call = AssertUnwindSafe(handler.handle(&chain.payload));
    let outcome = tokio::time::timeout(
        Duration::from_millis(chain.contract.timeout_ms),
        call.catch_unwind(),
    )
    .await;
    let result = match outcome {
        Err(_elapsed) => {
            return Err(format!(
                "handler exceeded the {}ms hook-point budget",
                chain.contract.timeout_ms
            ));
        }
        Ok(Err(panic)) => {
            return Err(format!(
                "handler panicked: {}",
                panic_diagnostic(panic.as_ref())
            ));
        }
        Ok(Ok(Err(error))) => return Err(error.to_string()),
        Ok(Ok(Ok(result))) => result,
    };
    adopt(chain, entry, result)
}

/// Run one around handler under the point's budget.
async fn run_around(
    chain: &Chain,
    entry: &Arc<HookEntry>,
    handler: &dyn AroundHookHandler,
    next: AroundNext<'_>,
) -> Result<HookOutcome, String> {
    let call = AssertUnwindSafe(handler.handle(&chain.payload, next));
    let outcome = tokio::time::timeout(
        Duration::from_millis(chain.contract.timeout_ms),
        call.catch_unwind(),
    )
    .await;
    let result = match outcome {
        Err(_elapsed) => {
            return Err(format!(
                "handler exceeded the {}ms hook-point budget",
                chain.contract.timeout_ms
            ));
        }
        Ok(Err(panic)) => {
            return Err(format!(
                "handler panicked: {}",
                panic_diagnostic(panic.as_ref())
            ));
        }
        Ok(Ok(Err(error))) => return Err(error.to_string()),
        Ok(Ok(Ok(result))) => result,
    };
    adopt(chain, entry, result)
}

/// Validate one handler output against the point's mode and apply it.
fn adopt(chain: &Chain, entry: &Arc<HookEntry>, result: HookResult) -> Result<HookOutcome, String> {
    let implementation = Some(entry.implementation_digest());
    let mut outcome = HookOutcome {
        hook_id: entry.spec.hook_id,
        result: result.clone(),
        error: None,
        implementation,
        adopted: true,
        allowed: true,
        modified: false,
        reason: None,
    };
    if !result.is_legal_for(chain.contract.mode) {
        // An illegal output is never adopted, whatever the failure policy says.
        // It is not a handler failure either: the dispatch continues.
        outcome.adopted = false;
        outcome.result = HookResult::NoChange;
        outcome.error = Some(format!(
            "{:?}-mode hook returned an output outside the point contract; output discarded",
            chain.contract.mode
        ));
        return Ok(outcome);
    }
    match result {
        HookResult::NoChange => {}
        HookResult::Defer { .. } => {}
        HookResult::Reject { code, details } => {
            outcome.allowed = false;
            outcome.reason = Some(format!("{code}: {details}"));
            chain.state.lock().stopped_early = true;
        }
        HookResult::Replace { value } => match apply_transform(chain, &value) {
            Ok(()) => outcome.modified = true,
            Err(diagnostic) => {
                outcome.adopted = false;
                outcome.result = HookResult::NoChange;
                outcome.error = Some(diagnostic);
            }
        },
        HookResult::Contribute { items } => {
            let mut violation = None;
            for item in &items {
                if !chain.contract.allows_slot(item.slot) {
                    violation = Some(format!(
                        "HOOK_WRITE_VIOLATION: slot {:?} is not writable at {}",
                        item.slot, chain.contract.point
                    ));
                    break;
                }
            }
            if let Some(diagnostic) = violation {
                outcome.adopted = false;
                outcome.result = HookResult::NoChange;
                outcome.error = Some(diagnostic);
            } else {
                match apply_contributions(chain, items) {
                    Ok(()) => outcome.modified = true,
                    Err(diagnostic) => {
                        outcome.adopted = false;
                        outcome.result = HookResult::NoChange;
                        outcome.error = Some(diagnostic);
                    }
                }
            }
        }
        HookResult::Decide { value } => {
            let mut state = chain.state.lock();
            if state.decision.is_none() {
                state.decision = Some(value);
                outcome.modified = true;
            }
        }
    }
    Ok(outcome)
}

/// Apply one transform replacement under the point's merge rule.
fn apply_transform(chain: &Chain, value: &serde_json::Value) -> Result<(), String> {
    let Some(object) = value.as_object() else {
        return Err("HOOK_WRITE_VIOLATION: a transform must return an object".into());
    };
    for key in object.keys() {
        if !chain.contract.allows_path(key) {
            return Err(format!(
                "HOOK_WRITE_VIOLATION: {key} is not writable at {}",
                chain.contract.point
            ));
        }
    }
    let mut state = chain.state.lock();
    let current = state
        .value
        .clone()
        .unwrap_or_else(|| chain.payload.initial_value());
    let merged = match chain.contract.transform_merge {
        TransformMerge::ReplaceWhole => value.clone(),
        TransformMerge::PatchFields => {
            let mut base = match current {
                serde_json::Value::Object(map) => map,
                _ => serde_json::Map::new(),
            };
            for (key, field) in object {
                base.insert(key.clone(), field.clone());
            }
            serde_json::Value::Object(base)
        }
    };
    state.value = Some(merged);
    Ok(())
}

/// Merge one handler's contributions under the shared merge rule.
fn apply_contributions(chain: &Chain, items: Vec<ContextContribution>) -> Result<(), String> {
    let mut grouped: BTreeMap<(ContributionSlot, String), Vec<ContextContribution>> = BTreeMap::new();
    for item in items {
        grouped
            .entry((item.slot, item.key.clone()))
            .or_default()
            .push(item);
    }
    for ((slot, key), contributions) in grouped {
        match merge_contributions(slot, &key, contributions) {
            Ok(merged) => {
                let mut state = chain.state.lock();
                match state
                    .contributions
                    .iter_mut()
                    .find(|existing| existing.slot == merged.slot && existing.key == merged.key)
                {
                    Some(existing) => *existing = merged,
                    None => state.contributions.push(merged),
                }
            }
            Err(ContributionConflict { producers, .. }) => {
                return Err(format!(
                    "CONTRIBUTION_CONFLICT: slot {slot:?} key {key} produced by {producers:?}"
                ));
            }
        }
    }
    Ok(())
}

fn panic_diagnostic(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = panic.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".into()
    }
}

impl Default for HookRegistry {
    fn default() -> Self {
        Self::new()
    }
}
