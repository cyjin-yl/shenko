//! Hook API — SPEC §37, §41.8.
//!
//! Deterministic injection order that does not depend on registration order,
//! schema-version admission, conflict/cycle rejection that leaves the running
//! graph untouched, explicit handler diagnostics, and an atomic
//! generation swap so an in-flight run keeps the graph it snapshot.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
use thiserror::Error;

/// Payload schema the registry currently admits. A hook compiled against a
/// different schema is refused instead of being interpreted against new fields.
pub const HOOK_SCHEMA_VERSION: u32 = 1;

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

/// Static description of one hook. It travels with the handler so the graph can
/// be validated and ordered before any handler runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSpec {
    pub hook_id: EntityId,
    pub plugin_id: EntityId,
    pub phase: HookPhase,
    pub name: String,
    pub schema_version: u32,
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
}

impl HookResult {
    pub fn allow() -> Self {
        Self {
            hook_id: EntityId::new(),
            allowed: true,
            modified: false,
            reason: None,
            error: None,
        }
    }
    pub fn block(reason: impl Into<String>) -> Self {
        Self {
            hook_id: EntityId::new(),
            allowed: false,
            modified: false,
            reason: Some(reason.into()),
            error: None,
        }
    }
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
}

/// The admitted graph. Replaced only as a whole, so a rejected registration
/// never leaves a half-applied graph behind.
#[derive(Default)]
struct Graph {
    generation: u64,
    /// Entries ordered by phase, dependency order and stable id.
    ordered: Vec<Arc<HookEntry>>,
}

impl Graph {
    fn build(generation: u64, entries: Vec<Arc<HookEntry>>) -> Result<Self, HookAdmissionError> {
        let mut by_id: BTreeMap<EntityId, usize> = BTreeMap::new();
        for (index, entry) in entries.iter().enumerate() {
            if entry.spec.schema_version != HOOK_SCHEMA_VERSION {
                return Err(HookAdmissionError::UnknownSchema {
                    hook_id: entry.spec.hook_id,
                    expected: HOOK_SCHEMA_VERSION,
                    found: entry.spec.schema_version,
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
        Ok(Self {
            generation,
            ordered,
        })
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
        let rebuilt = Graph::build(revision, entries)?;
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
        let rebuilt = Graph::build(revision, entries)?;
        self.publish(graph.generation, rebuilt)?;
        Ok(true)
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
        let rebuilt = Graph::build(generation, entries.into_iter().map(Arc::new).collect())?;
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

    pub async fn run_phase(&self, payload: &HookPayload) -> Vec<HookResult> {
        self.snapshot().run_phase(payload).await
    }

    /// Keep the existing observer policy: only an explicit block disallows.
    pub async fn check_allowed(&self, payload: &HookPayload) -> bool {
        self.run_phase(payload).await.iter().all(|r| r.allowed)
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

    pub async fn run_phase(&self, payload: &HookPayload) -> Vec<HookResult> {
        let phase = payload.phase();
        let entries = self
            .graph
            .ordered
            .iter()
            .filter(|entry| entry.spec.phase == phase);
        let mut results = Vec::with_capacity(self.graph.ordered.len());
        for entry in entries {
            match entry.handler.handle(payload).await {
                Ok(mut result) => {
                    result.hook_id = entry.spec.hook_id;
                    results.push(result);
                }
                Err(error) => {
                    tracing::warn!(hook=%entry.spec.hook_id, err=%error, "hook observer failed; output skipped");
                    results.push(HookResult {
                        hook_id: entry.spec.hook_id,
                        allowed: true,
                        modified: false,
                        reason: None,
                        error: Some(error.to_string()),
                    });
                }
            }
        }
        results
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
            after,
            before: Vec::new(),
            requires: Vec::new(),
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
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r| r.allowed && r.error.is_none()));

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
        assert_eq!(results.len(), 1);
        assert!(results[0].allowed);
        assert!(results[0].error.is_some());
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
        assert_eq!(old_results.len(), 1);
        assert_eq!(
            old_results[0].hook_id, old_id,
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
        assert_eq!(in_flight.await.unwrap()[0].hook_id, id);
        assert!(
            original_lifetime.upgrade().is_none(),
            "an ownership token must not retain a retired handler"
        );
        assert_eq!(log.lock().clone(), vec!["replacement", "original"]);
        assert!(registry.unregister(&_replacement.registrations[0]).unwrap());
        assert!(registry.order(HookPhase::BeforeInference).is_empty());
    }
}
