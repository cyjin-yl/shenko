//! Hook API — SPEC §37, §41.8.
//!
//! Deterministic injection order that does not depend on registration order,
//! schema-version admission, conflict/cycle rejection that leaves the running
//! graph untouched, explicit handler diagnostics, and an atomic
//! generation swap so an in-flight run keeps the graph it snapshot.
//!
//! Hook point contracts (mode, major version, per-handler timeout and failure
//! policy) are admitted into the graph itself, so an in-flight dispatch keeps
//! the contract it snapshot and a major-version bump fences incompatible
//! handlers rather than reinterpreting them. Every admitted graph carries a
//! deterministic `graph_digest` over its contracts and versioned handler
//! artifacts, so a retained dispatch receipt stays interpretable after
//! hot-updates and across restarts.
use std::collections::{BTreeMap, BTreeSet};
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
use shenko_types::object::{Digest, VersionedRef};
use thiserror::Error;

/// Payload schema the registry currently admits. A hook compiled against a
/// different schema is refused instead of being interpreted against new fields.
pub const HOOK_SCHEMA_VERSION: u32 = 1;

/// Major version of every built-in hook point in the standard install profile.
/// Handlers declare the point major they were compiled against; a mismatch is
/// refused at admission instead of being interpreted against a newer contract.
pub const STANDARD_HOOK_POINT_MAJOR: u32 = 1;

/// Per-handler budget of the standard install profile. SPEC §41.8 gives
/// optional contributions a bounded local preparation budget; an observer
/// hook that does not answer inside it is diagnosed, never awaited forever.
pub const STANDARD_HOOK_TIMEOUT_MS: u64 = 250;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HookPhase {
    BeforeInference = 10,
    AfterInference = 20,
    BeforeOperation = 30,
    AfterOperation = 40,
    CapabilityChanged = 50,
    GenerationHandoff = 60,
}

impl HookPhase {
    /// Every built-in hook point of the standard catalog.
    pub const ALL: [HookPhase; 6] = [
        HookPhase::BeforeInference,
        HookPhase::AfterInference,
        HookPhase::BeforeOperation,
        HookPhase::AfterOperation,
        HookPhase::CapabilityChanged,
        HookPhase::GenerationHandoff,
    ];
}

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
}

impl HookPayload {
    pub fn phase(&self) -> HookPhase {
        match self {
            HookPayload::BeforeInference { .. } => HookPhase::BeforeInference,
            HookPayload::AfterInference { .. } => HookPhase::AfterInference,
            HookPayload::BeforeOperation { .. } => HookPhase::BeforeOperation,
            HookPayload::AfterOperation { .. } => HookPhase::AfterOperation,
            HookPayload::CapabilityChanged { .. } => HookPhase::CapabilityChanged,
            HookPayload::GenerationHandoff { .. } => HookPhase::GenerationHandoff,
        }
    }
}

/// Synthesis contract of a hook point — SPEC §41.8.1 `mode`.
///
/// Only `Observe` is admitted today: dispatching modes that transform,
/// collect or decide requires §41.8.2 output synthesis that this registry
/// does not implement, so declaring one is refused rather than silently
/// honored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookMode {
    Observe,
    Transform,
    Collect,
    Decide,
    Around,
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

/// Declared contract of one hook point — SPEC §41.8.1 `HookPoint`.
///
/// The contract travels with the admitted graph: a snapshot keeps dispatching
/// under the contract it leased, and publishing a changed contract republishes
/// the whole graph through the same admission checks as a handler change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookPointContract {
    pub phase: HookPhase,
    /// Contract major version. Handlers bind one `point_major`; admission
    /// refuses a handler compiled for a different major of this point.
    pub major: u32,
    pub mode: HookMode,
    /// Finite per-handler budget. Zero is not a valid contract.
    pub timeout_ms: u64,
    pub failure: HookFailurePolicy,
}

impl HookPointContract {
    /// Standard install profile for one built-in point: observer contract,
    /// major 1, bounded 250ms per handler, report-and-continue.
    pub fn standard(phase: HookPhase) -> Self {
        Self {
            phase,
            major: STANDARD_HOOK_POINT_MAJOR,
            mode: HookMode::Observe,
            timeout_ms: STANDARD_HOOK_TIMEOUT_MS,
            failure: HookFailurePolicy::ReportAndContinue,
        }
    }
}

/// Static description of one hook. It travels with the handler so the graph can
/// be validated and ordered before any handler runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSpec {
    pub hook_id: EntityId,
    pub plugin_id: EntityId,
    pub phase: HookPhase,
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
    /// Required compatible handlers in the same phase, independent of order.
    #[serde(default)]
    pub requires: Vec<EntityId>,
}

#[derive(Clone)]
pub struct HookEntry {
    pub spec: HookSpec,
    pub handler: Arc<dyn HookHandler>,
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

#[async_trait]
pub trait HookHandler: Send + Sync + 'static {
    async fn handle(&self, payload: &HookPayload) -> anyhow::Result<HookResult>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookResult {
    pub hook_id: EntityId,
    pub allowed: bool,
    pub modified: bool,
    pub reason: Option<String>,
    /// Diagnostic for a failed observer; its output is skipped. This is not a
    /// new product-permission gate. Phase consumers keep their existing policy.
    #[serde(default)]
    pub error: Option<String>,
    /// Artifact digest of the handler that produced this result. Stamped by
    /// the dispatcher, not by the handler, so a retained receipt names the
    /// exact implementation that ran. `None` only on handler-constructed
    /// pre-dispatch values.
    #[serde(default)]
    pub implementation: Option<Digest>,
}

impl HookResult {
    pub fn allow() -> Self {
        Self {
            hook_id: EntityId::new(),
            allowed: true,
            modified: false,
            reason: None,
            error: None,
            implementation: None,
        }
    }
    pub fn block(reason: impl Into<String>) -> Self {
        Self {
            hook_id: EntityId::new(),
            allowed: false,
            modified: false,
            reason: Some(reason.into()),
            error: None,
            implementation: None,
        }
    }
}

/// Receipt of one phase dispatch — SPEC §41.8.1 `HookDispatch`.
///
/// It records the durable identity of what ran: the leased graph revision and
/// its digest, plus every handler result stamped with the artifact that
/// produced it. `stopped_early` means a `stop_unit` contract cut the dispatch
/// short; the failing result carries the diagnostic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookDispatch {
    pub dispatch_id: EntityId,
    pub phase: HookPhase,
    /// Runtime-only revision of the leased graph.
    pub generation: u64,
    /// Content identity of the leased graph: contracts plus member specs and
    /// implementations. Two graphs sharing a digest dispatch identically, even
    /// across restarts.
    pub graph_digest: Digest,
    pub results: Vec<HookResult>,
    pub stopped_early: bool,
}

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
    #[error("plugin {plugin_id} already owns hook name {name} in {phase:?}")]
    DuplicateName {
        plugin_id: EntityId,
        phase: HookPhase,
        name: String,
    },
    #[error("hook {hook_id} depends on {missing}, which is not an admitted hook of the same phase")]
    MissingDependency {
        hook_id: EntityId,
        missing: EntityId,
    },
    #[error("hook {hook_id} depends on {dependency} from a different phase")]
    CrossPhaseDependency {
        hook_id: EntityId,
        dependency: EntityId,
    },
    #[error("hook graph has a cycle; blocked handlers: {cycle:?}")]
    Cycle { cycle: Vec<EntityId> },
    #[error("hook graph revision exhausted")]
    RevisionExhausted,
    #[error("hook graph changed during candidate preparation: expected {expected}, found {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("hook {hook_id} was compiled for {phase:?} major {handler_major}; the admitted contract is major {point_major}")]
    PointMajorMismatch {
        hook_id: EntityId,
        phase: HookPhase,
        point_major: u32,
        handler_major: u32,
    },
    #[error("hook point mode {mode:?} is not admitted by this registry; only observe dispatch is implemented")]
    UnsupportedMode { mode: HookMode },
    #[error("hook point {phase:?} contract must give a finite timeout_ms > 0")]
    InvalidPointContract { phase: HookPhase },
}

/// The admitted graph. Replaced only as a whole, so a rejected registration
/// never leaves a half-applied graph behind.
struct Graph {
    generation: u64,
    /// Entries ordered by phase, dependency order and stable id.
    ordered: Vec<Arc<HookEntry>>,
    /// Declared contract of each hook point. Contracts are admitted with the
    /// graph, so a leased snapshot keeps dispatching under the contract it
    /// was taken from even after a profile republication.
    contracts: BTreeMap<HookPhase, HookPointContract>,
    /// Content identity of this graph: digest over contracts plus every
    /// member spec including its versioned implementation. Deterministic for
    /// identical declarations, independent of registration order or uptime.
    digest: Digest,
}

impl Graph {
    fn standard_contracts() -> BTreeMap<HookPhase, HookPointContract> {
        HookPhase::ALL
            .into_iter()
            .map(|phase| (phase, HookPointContract::standard(phase)))
            .collect()
    }

    fn compute_digest(
        contracts: &BTreeMap<HookPhase, HookPointContract>,
        members: &[&HookSpec],
    ) -> Digest {
        #[derive(Serialize)]
        struct GraphIdentity<'a> {
            schema_version: u32,
            contracts: &'a BTreeMap<HookPhase, HookPointContract>,
            members: &'a [&'a HookSpec],
        }
        let identity = GraphIdentity {
            schema_version: HOOK_SCHEMA_VERSION,
            contracts,
            members,
        };
        let bytes = serde_json::to_vec(&identity)
            .expect("hook graph identity is serializable");
        Digest::blake3(&bytes)
    }

    fn build(
        generation: u64,
        entries: Vec<Arc<HookEntry>>,
        contracts: BTreeMap<HookPhase, HookPointContract>,
    ) -> Result<Self, HookAdmissionError> {
        let mut by_id: BTreeMap<EntityId, usize> = BTreeMap::new();
        for (index, entry) in entries.iter().enumerate() {
            if entry.spec.schema_version != HOOK_SCHEMA_VERSION {
                return Err(HookAdmissionError::UnknownSchema {
                    hook_id: entry.spec.hook_id,
                    expected: HOOK_SCHEMA_VERSION,
                    found: entry.spec.schema_version,
                });
            }
            let contract = contracts
                .get(&entry.spec.phase)
                .expect("every declared hook phase has an admitted contract");
            if entry.spec.point_major != contract.major {
                return Err(HookAdmissionError::PointMajorMismatch {
                    hook_id: entry.spec.hook_id,
                    phase: entry.spec.phase,
                    point_major: contract.major,
                    handler_major: entry.spec.point_major,
                });
            }
            if by_id.insert(entry.spec.hook_id, index).is_some() {
                return Err(HookAdmissionError::DuplicateHook(entry.spec.hook_id));
            }
        }
        let mut edges = BTreeSet::new();
        let mut indegrees = vec![0usize; entries.len()];
        let mut successors = vec![Vec::new(); entries.len()];
        let mut names: BTreeSet<(EntityId, HookPhase, String)> = BTreeSet::new();
        for (current, entry) in entries.iter().enumerate() {
            if !names.insert((
                entry.spec.plugin_id,
                entry.spec.phase,
                entry.spec.name.clone(),
            )) {
                return Err(HookAdmissionError::DuplicateName {
                    plugin_id: entry.spec.plugin_id,
                    phase: entry.spec.phase,
                    name: entry.spec.name.clone(),
                });
            }
            for dependency in entry
                .spec
                .after
                .iter()
                .chain(&entry.spec.before)
                .chain(&entry.spec.requires)
            {
                let Some(&index) = by_id.get(dependency) else {
                    if entry.spec.requires.contains(dependency) {
                        return Err(HookAdmissionError::MissingDependency {
                            hook_id: entry.spec.hook_id,
                            missing: *dependency,
                        });
                    }
                    continue;
                };
                if entries[index].spec.phase != entry.spec.phase {
                    return Err(HookAdmissionError::CrossPhaseDependency {
                        hook_id: entry.spec.hook_id,
                        dependency: *dependency,
                    });
                }
            }
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
        let mut ready: BTreeSet<_> = entries
            .iter()
            .enumerate()
            .filter(|(index, _)| indegrees[*index] == 0)
            .map(|(index, entry)| (entry.spec.phase, entry.spec.hook_id, index))
            .collect();
        let mut order = Vec::with_capacity(entries.len());
        while let Some((_, _, index)) = ready.pop_first() {
            order.push(index);
            for &successor in &successors[index] {
                indegrees[successor] -= 1;
                if indegrees[successor] == 0 {
                    ready.insert((
                        entries[successor].spec.phase,
                        entries[successor].spec.hook_id,
                        successor,
                    ));
                }
            }
        }
        if order.len() != entries.len() {
            let cycle = entries
                .iter()
                .enumerate()
                .filter(|(index, _)| indegrees[*index] > 0)
                .map(|(_, entry)| entry.spec.hook_id)
                .collect();
            return Err(HookAdmissionError::Cycle { cycle });
        }
        let mut entries: Vec<_> = entries.into_iter().map(Some).collect();
        let ordered = order
            .into_iter()
            .map(|index| entries[index].take().unwrap())
            .collect();
        let digest = Self::compute_digest(
            &contracts,
            &ordered
                .iter()
                .map(|entry| &entry.spec)
                .collect::<Vec<_>>(),
        );
        Ok(Self {
            generation,
            ordered,
            contracts,
            digest,
        })
    }
}

impl Default for Graph {
    fn default() -> Self {
        let contracts = Self::standard_contracts();
        let digest = Self::compute_digest(&contracts, &[]);
        Self {
            generation: 0,
            ordered: Vec::new(),
            contracts,
            digest,
        }
    }
}

pub struct HookRegistry {
    graph: RwLock<Arc<Graph>>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self {
            graph: RwLock::new(Arc::new(Graph::default())),
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
        let mut entries = Vec::with_capacity(graph.ordered.len() + 1);
        entries.extend(graph.ordered.iter().cloned());
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
        if !graph.ordered.iter().any(|entry| Arc::ptr_eq(entry, &owned)) {
            return Ok(false);
        }
        let mut entries = Vec::with_capacity(graph.ordered.len() - 1);
        entries.extend(
            graph
                .ordered
                .iter()
                .filter(|entry| !Arc::ptr_eq(entry, &owned))
                .cloned(),
        );
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

    /// Admit or replace the declared contract of one built-in hook point.
    ///
    /// The whole graph is re-validated and republished: a major-version bump
    /// fences handlers still declaring the old major (their candidates are
    /// refused with `PointMajorMismatch`), while in-flight snapshots keep
    /// dispatching under the contract they leased. Publishing the identical
    /// contract is a no-op that keeps the current revision. Only `Observe`
    /// mode is admitted; declaring any other mode fails with
    /// `UnsupportedMode` rather than being silently honored.
    pub fn declare_point(
        &self,
        contract: HookPointContract,
    ) -> Result<(), HookAdmissionError> {
        if contract.mode != HookMode::Observe {
            return Err(HookAdmissionError::UnsupportedMode { mode: contract.mode });
        }
        if contract.timeout_ms == 0 {
            return Err(HookAdmissionError::InvalidPointContract {
                phase: contract.phase,
            });
        }
        let graph = Arc::clone(&self.graph.read());
        if graph.contracts.get(&contract.phase) == Some(&contract) {
            return Ok(());
        }
        let mut contracts = graph.contracts.clone();
        contracts.insert(contract.phase, contract);
        let generation = graph
            .generation
            .checked_add(1)
            .ok_or(HookAdmissionError::RevisionExhausted)?;
        let rebuilt = Graph::build(
            generation,
            graph.ordered.iter().cloned().collect(),
            contracts,
        )?;
        self.publish(graph.generation, rebuilt)
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
        let rebuilt = Graph::build(
            generation,
            entries.into_iter().map(Arc::new).collect(),
            graph.contracts.clone(),
        )?;
        let registrations = rebuilt
            .ordered
            .iter()
            .map(|entry| HookRegistration {
                hook_id: entry.spec.hook_id,
                entry: Arc::downgrade(entry),
            })
            .collect();
        self.publish(graph.generation, rebuilt)?;
        Ok(HookPublication {
            generation,
            registrations,
        })
    }

    fn publish(&self, expected: u64, candidate: Graph) -> Result<(), HookAdmissionError> {
        let mut graph = self.graph.write();
        if graph.generation != expected {
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

    pub fn list(&self) -> Vec<(String, HookPhase)> {
        self.graph
            .read()
            .ordered
            .iter()
            .map(|e| (e.spec.name.clone(), e.spec.phase))
            .collect()
    }

    /// Ordered hook ids for one phase, as the registry will run them.
    pub fn order(&self, phase: HookPhase) -> Vec<EntityId> {
        self.graph
            .read()
            .ordered
            .iter()
            .filter(|entry| entry.spec.phase == phase)
            .map(|entry| entry.spec.hook_id)
            .collect()
    }

    /// Lease one immutable graph for a whole invocation, including its later
    /// phases. The lease retains the old handlers through an intervening swap.
    pub fn snapshot(&self) -> HookSnapshot {
        HookSnapshot {
            graph: Arc::clone(&self.graph.read()),
        }
    }

    /// Durable content identity of the currently admitted graph.
    pub fn graph_digest(&self) -> Digest {
        self.graph.read().digest.clone()
    }

    pub async fn run_phase(&self, payload: &HookPayload) -> HookDispatch {
        self.snapshot().run_phase(payload).await
    }

    /// Keep the existing observer policy: only an explicit adopted block
    /// disallows. A dispatch cut short by a `stop_unit` contract does not
    /// prove the unit may proceed, so it reads as not allowed.
    pub async fn check_allowed(&self, payload: &HookPayload) -> bool {
        let dispatch = self.run_phase(payload).await;
        !dispatch.stopped_early && dispatch.results.iter().all(|r| r.allowed)
    }
}

#[derive(Clone)]
pub struct HookSnapshot {
    graph: Arc<Graph>,
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

    /// Dispatch one phase under the leased contract set.
    ///
    /// Each handler gets the point's finite `timeout_ms`. A timeout, error or
    /// panic becomes a diagnostic `HookResult` and is handled by the point's
    /// failure policy: `report_and_continue` skips the output and runs the
    /// rest of the dispatch; `stop_unit` stops the dispatch after recording
    /// it. Illegal verdicts from an observe-mode point (a block or a claimed
    /// modification) are never adopted — SPEC §41.8.2 — and are discarded
    /// with a diagnostic regardless of the failure policy.
    pub async fn run_phase(&self, payload: &HookPayload) -> HookDispatch {
        let phase = payload.phase();
        let contract = self
            .graph
            .contracts
            .get(&phase)
            .expect("every declared hook phase has an admitted contract");
        let entries = self
            .graph
            .ordered
            .iter()
            .filter(|entry| entry.spec.phase == phase);
        let mut results = Vec::with_capacity(self.graph.ordered.len());
        let mut stopped_early = false;
        for entry in entries {
            let handler = AssertUnwindSafe(entry.handler.handle(payload));
            let outcome = tokio::time::timeout(
                Duration::from_millis(contract.timeout_ms),
                handler.catch_unwind(),
            )
            .await;
            let failure: Option<String> = match outcome {
                Err(_elapsed) => Some(format!(
                    "handler exceeded the {}ms hook-point budget",
                    contract.timeout_ms
                )),
                Ok(Err(panic)) => Some(format!("handler panicked: {}", panic_diagnostic(&panic))),
                Ok(Ok(Err(error))) => Some(error.to_string()),
                Ok(Ok(Ok(mut result))) => {
                    result.hook_id = entry.spec.hook_id;
                    result.implementation = Some(entry.spec.implementation.artifact.digest.clone());
                    if contract.mode == HookMode::Observe && (!result.allowed || result.modified) {
                        // Illegal verdicts are never adopted, whatever the
                        // failure policy says; the handler keeps running next
                        // dispatch but this output is discarded.
                        results.push(HookResult {
                            hook_id: entry.spec.hook_id,
                            allowed: true,
                            modified: false,
                            reason: result.reason,
                            error: Some(
                                "observe-mode hook attempted a verdict or modification; output discarded"
                                    .into(),
                            ),
                            implementation: result.implementation,
                        });
                    } else {
                        results.push(result);
                    }
                    continue;
                }
            };
            let diagnostic = failure.expect("failure branch always yields a diagnostic");
            tracing::warn!(
                hook = %entry.spec.hook_id,
                err = %diagnostic,
                "hook observer failed; output skipped"
            );
            results.push(HookResult {
                hook_id: entry.spec.hook_id,
                allowed: true,
                modified: false,
                reason: None,
                error: Some(diagnostic),
                implementation: Some(entry.spec.implementation.artifact.digest.clone()),
            });
            if contract.failure == HookFailurePolicy::StopUnit {
                stopped_early = true;
                break;
            }
        }
        HookDispatch {
            dispatch_id: EntityId::new(),
            phase,
            generation: self.graph.generation,
            graph_digest: self.graph.digest.clone(),
            results,
            stopped_early,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use tokio::sync::Notify;

    use shenko_types::object::{ArtifactRef, SchemaRef};

    struct Recorder {
        name: &'static str,
        log: Arc<Mutex<Vec<&'static str>>>,
        gate: Option<(Arc<Notify>, Arc<Notify>)>,
        fail: bool,
    }

    #[async_trait]
    impl HookHandler for Recorder {
        async fn handle(&self, _payload: &HookPayload) -> anyhow::Result<HookResult> {
            if let Some((entered, release)) = &self.gate {
                entered.notify_one();
                release.notified().await;
            }
            self.log.lock().push(self.name);
            if self.fail {
                anyhow::bail!("handler exploded");
            }
            Ok(HookResult::allow())
        }
    }

    fn spec(plugin: EntityId, phase: HookPhase, name: &str, after: Vec<EntityId>) -> HookSpec {
        HookSpec {
            hook_id: EntityId::new(),
            plugin_id: plugin,
            phase,
            name: name.into(),
            schema_version: HOOK_SCHEMA_VERSION,
            point_major: STANDARD_HOOK_POINT_MAJOR,
            implementation: implementation(name),
            after,
            before: Vec::new(),
            requires: Vec::new(),
        }
    }

    /// Versioned handler identity bound to the declared label.
    fn implementation(label: &str) -> VersionedRef {
        let digest = Digest::blake3(format!("hook-impl/{label}").as_bytes());
        VersionedRef {
            artifact: ArtifactRef {
                digest: digest.clone(),
                locator: format!("test:hook/{label}"),
            },
            symbol: format!("hook.{label}.handle"),
            schema: SchemaRef {
                name: "hook-payload-v1".into(),
                major: HOOK_SCHEMA_VERSION,
                digest,
            },
        }
    }

    fn entry(spec: HookSpec, name: &'static str, log: Arc<Mutex<Vec<&'static str>>>) -> HookEntry {
        HookEntry {
            spec,
            handler: Arc::new(Recorder {
                name,
                log,
                gate: None,
                fail: false,
            }),
        }
    }

    fn payload() -> HookPayload {
        HookPayload::BeforeInference {
            request_id: EntityId::new(),
            agent_id: EntityId::new(),
            model: "fixture".into(),
        }
    }

    #[tokio::test]
    async fn graph_order_schema_conflicts_failure_and_generation_swap() {
        let phase = HookPhase::BeforeInference;
        let plugin = EntityId::new();
        let log = Arc::new(Mutex::new(Vec::new()));

        // Registration order carries no meaning: dependency edges and stable
        // ids decide, and the same set admitted in any order agrees.
        let registry = HookRegistry::new();
        let mut a_spec = spec(plugin, phase, "a", vec![]);
        a_spec.hook_id = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let a_id = a_spec.hook_id;
        let mut b_spec = spec(plugin, phase, "b", vec![a_id]);
        b_spec.requires.push(a_id);
        let mut c_spec = spec(plugin, phase, "c", vec![]);
        c_spec.hook_id = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let c_id = c_spec.hook_id;
        let admitted = registry
            .swap_generation(vec![
                entry(b_spec.clone(), "b", Arc::clone(&log)),
                entry(a_spec.clone(), "a", Arc::clone(&log)),
                entry(c_spec.clone(), "c", Arc::clone(&log)),
            ])
            .unwrap();
        let expected = vec![c_id, a_id, b_spec.hook_id];
        assert_eq!(registry.order(phase), expected);

        let reversed = HookRegistry::new();
        for (s, name) in [
            (c_spec.clone(), "c"),
            (a_spec.clone(), "a"),
            (b_spec.clone(), "b"),
        ] {
            reversed.register(entry(s, name, Arc::clone(&log))).unwrap();
        }
        assert_eq!(
            reversed.order(phase),
            expected,
            "registration order must not matter"
        );

        let results = registry.run_phase(&payload()).await;
        assert_eq!(log.lock().clone(), vec!["c", "a", "b"]);
        assert_eq!(results.results.len(), 3);
        assert!(
            results
                .results
                .iter()
                .all(|r| r.allowed && r.error.is_none())
        );
        assert!(!results.stopped_early);

        // Schema mismatch, duplicates, cycles and dangling dependencies are all
        // rejected without disturbing the admitted graph.
        log.lock().clear();
        let mut future_schema = spec(plugin, phase, "future", vec![]);
        future_schema.schema_version = HOOK_SCHEMA_VERSION + 1;
        assert!(matches!(
            registry.register(entry(future_schema, "future", Arc::clone(&log))),
            Err(HookAdmissionError::UnknownSchema { .. })
        ));
        assert!(matches!(
            registry.register(entry(a_spec.clone(), "a-again", Arc::clone(&log))),
            Err(HookAdmissionError::DuplicateHook(_))
        ));
        assert!(matches!(
            registry.register(entry(
                spec(plugin, phase, "a", vec![]),
                "shadow",
                Arc::clone(&log)
            )),
            Err(HookAdmissionError::DuplicateName { .. })
        ));
        let mut dangling = spec(plugin, phase, "dangling", vec![]);
        dangling.requires.push(EntityId::new());
        assert!(matches!(
            registry.register(entry(dangling, "dangling", Arc::clone(&log))),
            Err(HookAdmissionError::MissingDependency { .. })
        ));
        // A cycle can only appear in a whole-graph admission, so the swap is
        // where it must be refused.
        let mut cycle_one = spec(plugin, phase, "cycle-one", vec![]);
        let mut cycle_two = spec(plugin, phase, "cycle-two", vec![]);
        cycle_one.after = vec![cycle_two.hook_id];
        cycle_two.after = vec![cycle_one.hook_id];
        assert!(matches!(
            registry.swap_generation(vec![
                entry(cycle_one, "cycle-one", Arc::clone(&log)),
                entry(cycle_two, "cycle-two", Arc::clone(&log)),
            ]),
            Err(HookAdmissionError::Cycle { .. })
        ));
        assert_eq!(
            registry.order(phase),
            expected,
            "rejected admissions keep the old graph"
        );

        assert!(matches!(
            registry.unregister(
                admitted
                    .registrations
                    .iter()
                    .find(|registration| registration.hook_id() == a_id)
                    .unwrap()
            ),
            Err(HookAdmissionError::MissingDependency { .. })
        ));
        assert_eq!(registry.order(phase), expected);

        // Missing optional ordering targets do not make a unit unavailable.
        // A before edge still takes precedence over stable-id tie breaking.
        let optional = HookRegistry::new();
        let mut later = a_spec.clone();
        later.hook_id = "00000000-0000-0000-0000-000000000003".parse().unwrap();
        later.name = "later".into();
        later.after = vec![EntityId::new()];
        later.before = vec![c_id];
        optional
            .swap_generation(vec![
                entry(c_spec.clone(), "c", Arc::clone(&log)),
                entry(later.clone(), "later", Arc::clone(&log)),
            ])
            .unwrap();
        assert_eq!(optional.order(phase), vec![later.hook_id, c_id]);

        // A failed observer is visible without introducing a permission gate.
        let failing = HookRegistry::new();
        let fail_spec = spec(plugin, HookPhase::BeforeOperation, "gate", vec![]);
        failing
            .register(HookEntry {
                spec: fail_spec,
                handler: Arc::new(Recorder {
                    name: "gate",
                    log: Arc::clone(&log),
                    gate: None,
                    fail: true,
                }),
            })
            .unwrap();
        let operation = HookPayload::BeforeOperation {
            operation_id: EntityId::new(),
            operation_name: "send".into(),
        };
        let results = failing.run_phase(&operation).await;
        assert_eq!(results.results.len(), 1);
        assert!(results.results[0].allowed);
        assert!(results.results[0].error.is_some());
        assert!(failing.check_allowed(&operation).await);

        // Generation swap: a run that already snapshotted the old graph keeps
        // its handlers, while new runs see the replaced graph.
        let swapping = Arc::new(HookRegistry::new());
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let old_spec = spec(plugin, HookPhase::AfterInference, "old", vec![]);
        let old_id = old_spec.hook_id;
        swapping
            .swap_generation(vec![
                HookEntry {
                    spec: old_spec,
                    handler: Arc::new(Recorder {
                        name: "old",
                        log: Arc::clone(&log),
                        gate: Some((Arc::clone(&entered), Arc::clone(&release))),
                        fail: false,
                    }),
                },
                entry(
                    spec(plugin, HookPhase::BeforeInference, "old-before", vec![]),
                    "old-before",
                    Arc::clone(&log),
                ),
            ])
            .unwrap();
        log.lock().clear();
        let lease = swapping.snapshot();
        let after = HookPayload::AfterInference {
            request_id: EntityId::new(),
            stop_reason: "end_turn".into(),
            output_tokens: 1,
        };
        let in_flight = {
            let registry = Arc::clone(&swapping);
            let after = after.clone();
            tokio::spawn(async move { registry.run_phase(&after).await })
        };
        entered.notified().await;
        let new_spec = spec(plugin, HookPhase::AfterInference, "new", vec![]);
        let generation = swapping
            .swap_generation(vec![entry(new_spec.clone(), "new", Arc::clone(&log))])
            .unwrap();
        assert_eq!(generation.generation, 2);
        assert_eq!(
            swapping.order(HookPhase::AfterInference),
            vec![new_spec.hook_id]
        );
        release.notify_one();
        let old_results = in_flight.await.unwrap();
        assert_eq!(old_results.results.len(), 1);
        assert_eq!(
            old_results.results[0].hook_id, old_id,
            "in-flight run keeps the old snapshot"
        );
        assert_eq!(log.lock().clone(), vec!["old"]);
        lease.run_phase(&payload()).await;
        assert_eq!(log.lock().clone(), vec!["old", "old-before"]);
        swapping.run_phase(&after).await;
        assert_eq!(log.lock().clone(), vec!["old", "old-before", "new"]);
    }

    #[tokio::test]
    async fn stale_withdrawal_does_not_remove_a_reused_hook_id() {
        let registry = Arc::new(HookRegistry::new());
        let log = Arc::new(Mutex::new(Vec::new()));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let original = spec(
            EntityId::new(),
            HookPhase::BeforeInference,
            "original",
            vec![],
        );
        let id = original.hook_id;
        let original_handler = Arc::new(Recorder {
            name: "original",
            log: Arc::clone(&log),
            gate: Some((Arc::clone(&entered), Arc::clone(&release))),
            fail: false,
        });
        let original_lifetime = Arc::downgrade(&original_handler);
        let _old_registration = registry
            .register(HookEntry {
                spec: original.clone(),
                handler: original_handler,
            })
            .unwrap();
        let snapshot = registry.snapshot();
        let in_flight = tokio::spawn(async move { snapshot.run_phase(&payload()).await });
        entered.notified().await;
        let replacement = HookSpec {
            name: "replacement".into(),
            ..original
        };
        let _replacement = registry
            .swap_generation(vec![entry(
                replacement.clone(),
                "replacement",
                Arc::clone(&log),
            )])
            .unwrap();
        let revision = registry.generation();

        let foreign = HookRegistry::new();
        let _foreign_owner = foreign
            .register(entry(replacement, "foreign", Arc::clone(&log)))
            .unwrap();
        let foreign_revision = foreign.generation();
        assert!(!foreign.unregister(&_replacement.registrations[0]).unwrap());
        assert_eq!(foreign.order(HookPhase::BeforeInference), vec![id]);
        assert_eq!(foreign.generation(), foreign_revision);

        // Cleanup belongs to the original registration, not its successor
        // that deliberately retained the stable hook id.
        assert!(!registry.unregister(&_old_registration).unwrap());
        assert_eq!(registry.order(HookPhase::BeforeInference), vec![id]);
        assert_eq!(registry.generation(), revision);
        registry.run_phase(&payload()).await;
        release.notify_one();
        assert_eq!(in_flight.await.unwrap().results[0].hook_id, id);
        assert!(
            original_lifetime.upgrade().is_none(),
            "an ownership token must not retain a retired handler"
        );
        assert_eq!(log.lock().clone(), vec!["replacement", "original"]);
        assert!(registry.unregister(&_replacement.registrations[0]).unwrap());
        assert!(registry.order(HookPhase::BeforeInference).is_empty());
    }

    #[tokio::test]
    async fn point_contract_admission_is_versioned_and_fenced() {
        let plugin = EntityId::new();
        let log = Arc::new(Mutex::new(Vec::new()));
        let registry = HookRegistry::new();

        // The standard profile admits a handler whose declared point major
        // matches the contract; one compiled for another major is refused and
        // the running graph is left untouched.
        let mut old_major = spec(plugin, HookPhase::BeforeInference, "v0", vec![]);
        old_major.point_major = 0;
        assert!(matches!(
            registry.register(entry(old_major, "v0", Arc::clone(&log))),
            Err(HookAdmissionError::PointMajorMismatch { .. })
        ));
        let v1_spec = spec(plugin, HookPhase::BeforeInference, "v1", vec![]);
        let publication = registry
            .register(entry(
                v1_spec.clone(),
                "v1",
                Arc::clone(&log),
            ))
            .unwrap();
        let populated_digest = registry.graph_digest();
        let leased = registry.snapshot();

        // A contract publication re-validates every member. Bumping the point
        // major while an incompatible handler is registered is refused; the
        // leased snapshot keeps its own contract and dispatch identity.
        let mut major_two = HookPointContract::standard(HookPhase::BeforeInference);
        major_two.major = 2;
        assert!(matches!(
            registry.declare_point(major_two.clone()),
            Err(HookAdmissionError::PointMajorMismatch { .. })
        ));
        assert_eq!(leased.graph_digest(), &registry.graph_digest());
        assert_eq!(
            leased.run_phase(&payload()).await.graph_digest,
            populated_digest
        );

        // With the incompatible handler withdrawn the major bump is admitted;
        // the leased graph keeps its durable identity.
        assert!(registry.unregister(&publication).unwrap());
        registry.declare_point(major_two).unwrap();
        assert_ne!(registry.graph_digest(), populated_digest);
        assert_eq!(*leased.graph_digest(), populated_digest);

        // Only observe-mode points are admitted; declaring a transform point
        // fails instead of dispatching under a contract without synthesis.
        let mut transform = HookPointContract::standard(HookPhase::BeforeOperation);
        transform.mode = HookMode::Transform;
        assert!(matches!(
            registry.declare_point(transform),
            Err(HookAdmissionError::UnsupportedMode { .. })
        ));
        let mut zero = HookPointContract::standard(HookPhase::BeforeOperation);
        zero.timeout_ms = 0;
        assert!(matches!(
            registry.declare_point(zero),
            Err(HookAdmissionError::InvalidPointContract { .. })
        ));

        // A fresh registry admitting identical declarations reproduces the
        // same graph digest: the digest is durable content identity, not a
        // per-process or uptime-scoped id, and identical declarations across
        // restarts dispatch identically.
        let twin = HookRegistry::new();
        twin.register(entry(v1_spec, "v1", Arc::clone(&log))).unwrap();
        assert_eq!(twin.graph_digest(), populated_digest);
    }

    #[tokio::test]
    async fn dispatch_honors_timeout_and_failure_policy() {
        let plugin = EntityId::new();
        let log = Arc::new(Mutex::new(Vec::new()));

        // Never answers: the dispatcher's bounded point budget is the only
        // thing that can end this handler's wait.
        struct Pending;
        #[async_trait]
        impl HookHandler for Pending {
            async fn handle(&self, _payload: &HookPayload) -> anyhow::Result<HookResult> {
                std::future::pending::<()>().await;
                unreachable!()
            }
        }

        // report_and_continue: a handler past the point budget is diagnosed
        // and the next handler still runs inside the same dispatch.
        let registry = HookRegistry::new();
        let mut quick = HookPointContract::standard(HookPhase::BeforeInference);
        quick.timeout_ms = 5;
        registry.declare_point(quick).unwrap();
        let mut slow = spec(plugin, HookPhase::BeforeInference, "slow", vec![]);
        slow.hook_id = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let mut fast = spec(plugin, HookPhase::BeforeInference, "fast", vec![]);
        fast.hook_id = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        registry
            .swap_generation(vec![
                HookEntry {
                    spec: slow.clone(),
                    handler: Arc::new(Pending),
                },
                entry(fast.clone(), "fast", Arc::clone(&log)),
            ])
            .unwrap();
        let dispatch = registry.run_phase(&payload()).await;
        assert_eq!(dispatch.results.len(), 2);
        assert!(dispatch.results[0]
            .error
            .as_deref()
            .unwrap()
            .contains("budget"));
        assert_eq!(
            dispatch.results[0].implementation,
            Some(slow.implementation.artifact.digest.clone()),
            "the receipt names the artifact that timed out"
        );
        assert!(dispatch.results[1].error.is_none());
        assert!(!dispatch.stopped_early);
        assert_eq!(log.lock().clone(), vec!["fast"]);
        assert_eq!(dispatch.graph_digest, registry.graph_digest());

        // stop_unit: a failing handler ends the dispatch; the remaining
        // handler does not run, and the unit does not read as allowed.
        let stopping = HookRegistry::new();
        let mut stop_contract = HookPointContract::standard(HookPhase::BeforeOperation);
        stop_contract.failure = HookFailurePolicy::StopUnit;
        stopping.declare_point(stop_contract).unwrap();
        let mut fragile = spec(plugin, HookPhase::BeforeOperation, "fragile", vec![]);
        fragile.hook_id = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let mut queued = spec(plugin, HookPhase::BeforeOperation, "queued", vec![]);
        queued.hook_id = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        log.lock().clear();
        stopping
            .swap_generation(vec![
                HookEntry {
                    spec: fragile,
                    handler: Arc::new(Recorder {
                        name: "fragile",
                        log: Arc::clone(&log),
                        gate: None,
                        fail: true,
                    }),
                },
                entry(queued, "queued", Arc::clone(&log)),
            ])
            .unwrap();
        let operation = HookPayload::BeforeOperation {
            operation_id: EntityId::new(),
            operation_name: "send".into(),
        };
        let dispatch = stopping.run_phase(&operation).await;
        assert!(dispatch.stopped_early);
        assert_eq!(dispatch.results.len(), 1);
        assert!(dispatch.results[0].error.is_some());
        assert!(log.lock().is_empty(), "the queued handler never ran");
        assert!(
            !stopping.check_allowed(&operation).await,
            "a stopped dispatch must not read as allowed"
        );

        // A panicking observer is contained like an error: diagnostic recorded,
        // report_and_continue runs the rest of the dispatch.
        struct Panic;
        #[async_trait]
        impl HookHandler for Panic {
            async fn handle(&self, _payload: &HookPayload) -> anyhow::Result<HookResult> {
                panic!("observer exploded");
            }
        }
        let panicking = HookRegistry::new();
        let mut first = spec(plugin, HookPhase::AfterInference, "panic", vec![]);
        first.hook_id = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let mut second = spec(plugin, HookPhase::AfterInference, "calm", vec![]);
        second.hook_id = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        log.lock().clear();
        panicking
            .swap_generation(vec![
                HookEntry {
                    spec: first,
                    handler: Arc::new(Panic),
                },
                entry(second, "calm", Arc::clone(&log)),
            ])
            .unwrap();
        let after = HookPayload::AfterInference {
            request_id: EntityId::new(),
            stop_reason: "end_turn".into(),
            output_tokens: 1,
        };
        let dispatch = panicking.run_phase(&after).await;
        assert_eq!(dispatch.results.len(), 2);
        assert!(dispatch.results[0]
            .error
            .as_deref()
            .unwrap()
            .contains("panicked"));
        assert!(dispatch.results[1].error.is_none());
        assert!(!dispatch.stopped_early);
        assert_eq!(log.lock().clone(), vec!["calm"]);

        // An observe-mode verdict is never adopted: a block result is
        // discarded with a diagnostic and does not disallow the unit.
        struct Blocker;
        #[async_trait]
        impl HookHandler for Blocker {
            async fn handle(&self, _payload: &HookPayload) -> anyhow::Result<HookResult> {
                Ok(HookResult::block("not adopted"))
            }
        }
        let observing = HookRegistry::new();
        observing
            .register(HookEntry {
                spec: spec(plugin, HookPhase::BeforeInference, "blocker", vec![]),
                handler: Arc::new(Blocker),
            })
            .unwrap();
        let dispatch = observing.run_phase(&payload()).await;
        assert_eq!(dispatch.results.len(), 1);
        assert!(dispatch.results[0].allowed);
        assert!(dispatch.results[0].error.is_some());
        assert!(observing.check_allowed(&payload()).await);
    }
}
