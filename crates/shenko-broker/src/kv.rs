//! Context residency pool and leases — SPEC §13.1–13.3, §13.7.3, §41.10.
//!
//! Compute permits and context residency are two different resources. This
//! module owns only the second: how much of a backend's context pool a
//! subject context and the children collective may keep, and how that share
//! moves when the workload changes.
//!
//! The main/child split is an outcome of the policy, not an input. The policy
//! enumerates candidate allocations — demand-proportional, symmetric and
//! neighbours of the current one — and picks the cheapest under queueing,
//! rebuild and transfer costs. Under a symmetric workload the demand
//! proportional candidate yields the even split; when the workload changes the
//! split follows it. No ratio is hard-coded, and no manual half-pool switch
//! exists.
//!
//! A pool whose capacity was never observed stays unknown. Entitlement numbers
//! computed from demand are then a soft claim, never a physical guarantee.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;

use crate::admission::CapacityProvenance;
use crate::capability::{
    BackendCapabilitySnapshot, NATIVE_NINFER_PROMPT_CACHE_KEY, NATIVE_STRATA_CONVERSATION_PARKING,
    NATIVE_STRATA_SLOT_SAVE_RESTORE, NATIVE_VLLM_CACHE_SALT, NATIVE_VLLM_KV_TRANSFER_PARAMS,
    RESIDENCY_WIRE_OPTIONS, is_residency_option,
};

/// Which side of the subject/children split a lease belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseSide {
    /// The subject's own contexts. External interruptions belong to the
    /// subject: they are the subject's own work, not a third competitor.
    Main,
    /// The children's collective.
    Child,
    /// An external interruption handled by the subject.
    External,
}

impl LeaseSide {
    /// The share group this side competes in.
    pub fn share_group(&self) -> ShareGroup {
        match self {
            Self::Main | Self::External => ShareGroup::Subject,
            Self::Child => ShareGroup::Children,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareGroup {
    Subject,
    Children,
}

/// The lifecycle state of one context lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    Requested,
    Reserving,
    Resident,
    Parked,
    Transferring,
    ReclaimPending,
    Invalid,
    Released,
}

/// How strong the residency promise behind a lease is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionGuarantee {
    /// A hint only; the backend may reuse or drop the context.
    Advisory,
    /// The backend keeps the context when it can, without exclusivity.
    BestEffort,
    /// The backend acknowledged the reservation.
    Acknowledged,
    /// The backend reserved the residency for this caller.
    Reserved,
}

/// The accounting of one lease's share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retention {
    pub guarantee: RetentionGuarantee,
    /// The share the policy granted this side.
    pub entitled_units: u64,
    /// The units actually resident.
    pub used_units: u64,
    /// Units used above the entitlement, borrowed from idle capacity.
    pub borrowed_units: u64,
    /// Units that are being reclaimed and are not available yet.
    pub reclaim_pending_units: u64,
}

/// The identity one context handle is only valid under (SPEC §13.5).
///
/// A backend restart, a model or tokenizer switch, a template or toolset change
/// or a different scope invalidates the handle. Every field is optional because
/// an unobserved value is unknown: unknown never proves a match, and a defaulted
/// historical identity therefore never implies residency.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResidencyIdentity {
    /// The deployment's identity of the physical engine or device pool.
    #[serde(default)]
    pub engine_identity: Option<String>,
    #[serde(default)]
    pub engine_incarnation: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub tokenizer_fingerprint: Option<String>,
    #[serde(default)]
    pub template_fingerprint: Option<String>,
    #[serde(default)]
    pub toolset_digest: Option<String>,
}

impl ResidencyIdentity {
    /// Whether any value was actually observed. A handle carried over from a
    /// state that had no identity is not evidence of residency.
    pub fn is_observed(&self) -> bool {
        self.engine_identity.is_some()
            || self.engine_incarnation.is_some()
            || self.model.is_some()
            || self.tokenizer_fingerprint.is_some()
            || self.template_fingerprint.is_some()
            || self.toolset_digest.is_some()
    }

    /// Whether two identities are compatible: every value known on both sides
    /// is equal. A value known on one side only cannot prove a change, so it
    /// does not invalidate the handle by itself.
    pub fn matches(&self, other: &Self) -> bool {
        let same = |left: &Option<String>, right: &Option<String>| match (left, right) {
            (Some(left), Some(right)) => left == right,
            _ => true,
        };
        same(&self.engine_identity, &other.engine_identity)
            && same(&self.engine_incarnation, &other.engine_incarnation)
            && same(&self.model, &other.model)
            && same(&self.tokenizer_fingerprint, &other.tokenizer_fingerprint)
            && same(&self.template_fingerprint, &other.template_fingerprint)
            && same(&self.toolset_digest, &other.toolset_digest)
    }

    /// Adopt what one observation established, keeping everything it left
    /// unknown.
    ///
    /// A value the observation did not establish proves nothing: it never
    /// erases a value that was already known. A known different value is a real
    /// change and replaces the old one, which is what invalidates the handles
    /// carrying it.
    pub fn adopt(&mut self, observed: &Self) {
        fn take(target: &mut Option<String>, observed: &Option<String>) {
            if let Some(value) = observed {
                *target = Some(value.clone());
            }
        }
        take(&mut self.engine_identity, &observed.engine_identity);
        take(&mut self.engine_incarnation, &observed.engine_incarnation);
        take(&mut self.model, &observed.model);
        take(&mut self.tokenizer_fingerprint, &observed.tokenizer_fingerprint);
        take(&mut self.template_fingerprint, &observed.template_fingerprint);
        take(&mut self.toolset_digest, &observed.toolset_digest);
    }

    /// The backend-level identity one capability snapshot established.
    ///
    /// Only the values the snapshot actually carries are filled in; the rest
    /// stay unknown so a defaulted field never proves residency.
    pub fn from_snapshot(snapshot: &BackendCapabilitySnapshot) -> Self {
        Self {
            engine_identity: None,
            engine_incarnation: snapshot.engine_incarnation.clone(),
            model: None,
            tokenizer_fingerprint: snapshot.tokenizer_fingerprint.clone(),
            template_fingerprint: snapshot.template_fingerprint.clone(),
            toolset_digest: snapshot.toolset_digest.clone(),
        }
    }
}

/// One context lease inside the residency pool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextLease {
    pub context_id: String,
    #[serde(default)]
    pub owner_task_id: Option<EntityId>,
    pub side: LeaseSide,
    pub state: LeaseState,
    /// The units this context needs to stay resident.
    pub required_units: u64,
    /// The upper bound of the prefix that can be reused instead of replayed.
    #[serde(default)]
    pub reusable_units: u64,
    /// The units that must be recomputed when the context is rebuilt.
    #[serde(default)]
    pub recompute_units: u64,
    /// Estimated probability that this context is used again soon.
    pub return_probability: f64,
    /// Higher priority contexts keep their residency longer.
    pub priority: u8,
    pub retention: Retention,
    /// The engine/model/template identity this lease was established under.
    #[serde(default)]
    pub identity: ResidencyIdentity,
    /// Readers currently using this context. Its units stay charged until they
    /// drain: a reader still draining is not free space (§13.3 step 6).
    #[serde(default)]
    pub readers: u32,
    /// Residency options this context's sealed request named that the backend
    /// never demonstrated. Present means the lease makes no residency claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity_gap: Option<String>,
    pub lease_revision: u64,
}

impl ContextLease {
    pub fn new(context_id: impl Into<String>, side: LeaseSide, required_units: u64) -> Self {
        Self {
            context_id: context_id.into(),
            owner_task_id: None,
            side,
            state: LeaseState::Requested,
            required_units,
            reusable_units: 0,
            recompute_units: required_units,
            return_probability: 0.5,
            priority: 0,
            retention: Retention {
                guarantee: RetentionGuarantee::Advisory,
                entitled_units: 0,
                used_units: 0,
                borrowed_units: 0,
                reclaim_pending_units: 0,
            },
            identity: ResidencyIdentity::default(),
            readers: 0,
            affinity_gap: None,
            lease_revision: 0,
        }
    }
}

/// The residency pool of one physical domain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KvPoolState {
    /// The observed pool capacity. `None` means it was never observed.
    pub capacity_units: Option<u64>,
    pub provenance: CapacityProvenance,
    /// The residency controls the bound backend actually demonstrated. A pool
    /// whose capabilities were never adopted carries the conservative default.
    #[serde(default)]
    pub capabilities: KvCapabilities,
    /// The backend-level identity the last discovery established (engine
    /// identity, engine incarnation and the model/tokenizer/template/toolset
    /// fingerprints). A lease carries the merge of this identity and the
    /// request-level facts, so an engine rebuild or a template change
    /// invalidates its handles. Nothing is filled in here: a field the probe
    /// never established stays unknown.
    #[serde(default)]
    pub identity: ResidencyIdentity,
    /// Requests whose provider outcome is unknown while their read hold on a
    /// context is still open, keyed by request id and naming the context
    /// (SPEC §41.9 unknown, §41.10). The units stay charged and the context
    /// stays off the eviction list until the original terminal evidence
    /// arrives and the hold is drained: a dropped connection, a deadline or a
    /// lost receipt is not proof that the backend stopped reading that context.
    ///
    /// The record is a request → context fact, never a residency claim: it is
    /// written only for a context that already had a lease, so it can never
    /// invent occupancy for a context nothing was observed about.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unknown_reads: BTreeMap<String, String>,
    pub leases: BTreeMap<String, ContextLease>,
}

impl KvPoolState {
    pub fn new(capacity_units: Option<u64>, provenance: CapacityProvenance) -> Self {
        Self {
            capacity_units,
            provenance,
            capabilities: KvCapabilities::default(),
            identity: ResidencyIdentity::default(),
            unknown_reads: BTreeMap::new(),
            leases: BTreeMap::new(),
        }
    }

    /// Units currently resident.
    pub fn used_units(&self) -> u64 {
        self.leases
            .values()
            .filter(|lease| matches!(lease.state, LeaseState::Resident | LeaseState::Transferring))
            .map(|lease| lease.required_units)
            .sum()
    }

    /// Units that are being reclaimed; they are not free yet.
    pub fn reclaim_pending_units(&self) -> u64 {
        self.leases
            .values()
            .filter(|lease| lease.state == LeaseState::ReclaimPending)
            .map(|lease| lease.required_units)
            .sum()
    }

    /// Units another context cannot use: resident, transferring and
    /// reclaim-pending. Reclaim-pending is counted because the backend has not
    /// confirmed the release yet; a context whose readers are still draining
    /// stays in the occupied set until they finish (§13.3 step 6).
    pub fn occupied_units(&self) -> u64 {
        self.leases
            .values()
            .filter(|lease| {
                matches!(
                    lease.state,
                    LeaseState::Resident | LeaseState::Transferring | LeaseState::ReclaimPending
                )
            })
            .map(|lease| lease.required_units)
            .sum()
    }

    /// Whether one context is still named by an unresolved unknown outcome.
    /// Its read hold is not free space, so nothing may release or evict it.
    pub fn has_unknown_read(&self, context_id: &str) -> bool {
        self.unknown_reads
            .values()
            .any(|context| context == context_id)
    }

    /// Invalidate every handle whose identity this engine/model/template no
    /// longer matches. The records stay: the old fact is kept, it is only no
    /// longer counted as residency of the current engine (SPEC §13.5).
    pub fn invalidate_stale(&mut self, identity: &ResidencyIdentity) -> usize {
        let mut invalidated = 0;
        for lease in self.leases.values_mut() {
            if matches!(lease.state, LeaseState::Invalid | LeaseState::Released) {
                continue;
            }
            // A handle with no observed identity came from state that could not
            // prove residency, so it may not be counted as resident now.
            if lease.identity.is_observed() && lease.identity.matches(identity) {
                continue;
            }
            lease.state = LeaseState::Invalid;
            lease.retention.used_units = 0;
            lease.retention.reclaim_pending_units = 0;
            lease.lease_revision += 1;
            invalidated += 1;
        }
        invalidated
    }

    /// The current entitlement of one share group.
    pub fn entitled_units(&self, group: ShareGroup) -> u64 {
        self.leases
            .values()
            .filter(|lease| lease.side.share_group() == group)
            .map(|lease| lease.retention.entitled_units)
            .max()
            .unwrap_or(0)
    }

    /// The demand of one share group: what the group wants to keep resident.
    pub fn demand_units(&self, group: ShareGroup) -> u64 {
        self.leases
            .values()
            .filter(|lease| lease.side.share_group() == group)
            .map(|lease| lease.required_units)
            .sum()
    }
}

/// What one caller asks the pool for.
#[derive(Debug, Clone, PartialEq)]
pub struct LeaseDemand {
    pub context_id: String,
    pub side: LeaseSide,
    /// The units the context needs resident.
    pub window_units: u64,
    /// The prefix the backend actually reported as reused on this call. It is
    /// an observation about one completion, never proof that the backend still
    /// retains KV after the context was released.
    pub reusable_units: u64,
    /// The units that must be recomputed.
    pub recompute_units: u64,
    pub return_probability: f64,
    pub priority: u8,
    /// The identity this handle is valid under.
    pub identity: ResidencyIdentity,
    /// The residency options the sealed request names. Each one needs a
    /// demonstrated backend operation before the lease may claim residency.
    pub affinity_options: Vec<String>,
}

/// What the backend really offers for residency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KvCapabilities {
    /// The backend has real retention/quota control. Only then may a lease
    /// claim a hard reservation.
    pub hard_retention: bool,
    pub supports_pause: bool,
    pub supports_spill: bool,
    pub supports_fork: bool,
    /// Cost of moving a context between pools, in pool units.
    pub transfer_cost_units: u64,
    /// Cost of rebuilding one unit that could not be reused.
    pub rebuild_cost_units: u64,
    /// Residency-affecting wire options this binding demonstrated. An option
    /// that was never demonstrated stays false: a request that names it is not
    /// a residency claim even when the backend accepted the field.
    #[serde(default)]
    pub cache_salt: bool,
    #[serde(default)]
    pub prompt_cache_key: bool,
    #[serde(default)]
    pub kv_transfer_params: bool,
}

impl Default for KvCapabilities {
    fn default() -> Self {
        Self {
            hard_retention: false,
            supports_pause: false,
            supports_spill: false,
            supports_fork: false,
            transfer_cost_units: 1,
            rebuild_cost_units: 1,
            cache_salt: false,
            prompt_cache_key: false,
            kv_transfer_params: false,
        }
    }
}

impl KvCapabilities {
    /// Project one capability snapshot onto the residency controls the pool may
    /// rely on.
    ///
    /// Only operations the backend demonstrated in its own probe evidence can
    /// raise a control. A pause/spill/fork control therefore stays false until
    /// the backend declared the operation; hard retention additionally needs an
    /// engine or device-pool scope with an acknowledged or reserved guarantee,
    /// which no request-level field can produce. Costs remain the conservative
    /// defaults because no probe measured a real transfer or rebuild cost.
    pub fn from_snapshot(snapshot: &BackendCapabilitySnapshot) -> Self {
        Self {
            hard_retention: snapshot.has_hard_retention(),
            // Host conversation parking and slot save/restore are pauses and
            // spills with their own scope; they are not GPU residency claims.
            supports_pause: snapshot.supports(NATIVE_STRATA_CONVERSATION_PARKING),
            supports_spill: snapshot.supports(NATIVE_STRATA_SLOT_SAVE_RESTORE),
            // No probed transport declares a context fork operation.
            supports_fork: false,
            transfer_cost_units: 1,
            rebuild_cost_units: 1,
            cache_salt: snapshot.supports(NATIVE_VLLM_CACHE_SALT),
            prompt_cache_key: snapshot.supports(NATIVE_NINFER_PROMPT_CACHE_KEY),
            kv_transfer_params: snapshot.supports(NATIVE_VLLM_KV_TRANSFER_PARAMS),
        }
    }

    /// Whether this binding demonstrated the operation one residency wire
    /// option depends on. A non-residency option is never a residency claim.
    pub fn demonstrates(&self, option: &str) -> bool {
        let Some(entry) = RESIDENCY_WIRE_OPTIONS
            .iter()
            .find(|entry| entry.option == option)
        else {
            return false;
        };
        match entry.operation {
            NATIVE_VLLM_CACHE_SALT => self.cache_salt,
            NATIVE_NINFER_PROMPT_CACHE_KEY => self.prompt_cache_key,
            NATIVE_VLLM_KV_TRANSFER_PARAMS => self.kv_transfer_params,
            _ => false,
        }
    }
}

/// The allocation the policy chose for one cycle.
#[derive(Debug, Clone, PartialEq)]
pub struct AllocationPlan {
    /// The subject's share. `None` when the pool capacity is unknown.
    pub subject_units: Option<u64>,
    /// The children's share.
    pub children_units: Option<u64>,
    pub guarantee: RetentionGuarantee,
    /// Why this allocation was chosen.
    pub rationale: String,
    /// How many candidates were evaluated.
    pub candidates_evaluated: usize,
}

/// The replaceable allocation policy.
pub trait KvPolicy: Send + Sync {
    fn plan(&self, pool: &KvPoolState, capabilities: &KvCapabilities) -> AllocationPlan;
}

/// The default automatic policy.
///
/// It enumerates a bounded candidate set and scores it. The symmetric split is
/// one candidate among them, exactly as the SPEC requires; it wins when the
/// workload is symmetric because the demand-proportional candidate produces
/// the same split there, and it loses as soon as the demands differ.
///
/// A share is only changed when the change is worth it: inside the minimum
/// hold window a drift smaller than the hysteresis band does not move the
/// split, so two nearly equal costs do not make the pool oscillate. A
/// materially better split (a side's demand outgrowing its share) switches
/// immediately, and a changed pool size invalidates the hold — physical safety
/// is never subject to the benefit threshold (§13.3 step 5).
pub struct AutomaticKvPolicy {
    /// Minimum time a share is kept before it may drift, in milliseconds.
    pub min_hold_ms: u64,
    /// Relative change below which the current share is kept.
    pub hysteresis_ratio: f64,
    /// The share the last plan chose. It is interior state because the policy
    /// is consulted through `&self` by the allocator that owns it.
    hold: parking_lot::Mutex<Option<ShareHold>>,
    /// The clock the hold window is measured on. It is injected so a test can
    /// drive the window without sleeping.
    clock: fn() -> u64,
}

/// One remembered allocation decision, used only to age the hold window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShareHold {
    subject_units: u64,
    capacity_units: u64,
    chosen_at_ms: u64,
}

impl Default for AutomaticKvPolicy {
    fn default() -> Self {
        Self {
            min_hold_ms: 30_000,
            hysteresis_ratio: 0.05,
            hold: parking_lot::Mutex::new(None),
            clock: wall_clock_ms,
        }
    }
}

impl AutomaticKvPolicy {
    /// A policy whose hold window is measured on the caller's clock.
    pub fn with_clock(min_hold_ms: u64, hysteresis_ratio: f64, clock: fn() -> u64) -> Self {
        Self {
            min_hold_ms,
            hysteresis_ratio,
            hold: parking_lot::Mutex::new(None),
            clock,
        }
    }

    /// Keep the previous subject share while the drift is only noise.
    ///
    /// The hold applies only to the same observed pool: a pool that grew or
    /// shrank is a new physical fact, so the split is recomputed instead of
    /// being carried over. Returns the share to use and whether it was held.
    fn stabilise(&self, capacity: u64, candidate: u64) -> (u64, bool) {
        let now = (self.clock)();
        let noise = (capacity as f64 * self.hysteresis_ratio) as u64;
        let mut hold = self.hold.lock();
        if let Some(held) = *hold {
            let same_pool = held.capacity_units == capacity;
            let fresh = now.saturating_sub(held.chosen_at_ms) < self.min_hold_ms;
            if same_pool && fresh && held.subject_units.abs_diff(candidate) <= noise {
                return (held.subject_units, true);
            }
        }
        *hold = Some(ShareHold {
            subject_units: candidate,
            capacity_units: capacity,
            chosen_at_ms: now,
        });
        (candidate, false)
    }

    /// The candidate shares of the subject, with the cost of each.
    ///
    /// Every candidate comes from the observed capacity and the current
    /// demand; no capability picks the split (capabilities only decide the
    /// guarantee a chosen split may claim).
    fn candidates(&self, pool: &KvPoolState) -> Vec<(u64, u64)> {
        let Some(capacity) = pool.capacity_units else { return Vec::new() };
        let subject_demand = pool.demand_units(ShareGroup::Subject);
        let children_demand = pool.demand_units(ShareGroup::Children);
        let total_demand = subject_demand + children_demand;
        let mut candidates: Vec<u64> = Vec::new();
        if total_demand > 0 {
            // Demand proportional: the split follows the actual demand.
            let proportional =
                (capacity as u128 * subject_demand as u128 / total_demand as u128) as u64;
            candidates.push(proportional.min(capacity));
        }
        // The symmetric candidate. It is one candidate, never the answer by
        // construction.
        candidates.push(capacity / 2);
        // Neighbours of the current share, so a small drift does not jump.
        let current = pool.entitled_units(ShareGroup::Subject);
        for step in [0.1_f64, 0.2_f64] {
            let delta = (capacity as f64 * step) as u64;
            candidates.push(current.saturating_add(delta).min(capacity));
            candidates.push(current.saturating_sub(delta));
        }
        candidates.push(current.min(capacity));
        candidates.retain(|value| *value <= capacity);
        candidates.sort_unstable();
        candidates.dedup();
        candidates
            .into_iter()
            .map(|subject| (subject, capacity - subject))
            .collect()
    }

    /// The weighted cost of one candidate split.
    ///
    /// The cost is the SPEC's `resume_cost` decomposition: uncovered
    /// prefill/replay plus the transfer of contexts that actually move. Idle
    /// entitlement is a cap, not a cost, and a share above a side's demand does
    /// not invent migration work — otherwise a pool much larger than the real
    /// demand makes every covering candidate look equally expensive and the
    /// cheapest plan could starve a side that has demand.
    fn cost(&self, pool: &KvPoolState, capabilities: &KvCapabilities, subject: u64) -> u64 {
        let capacity = pool.capacity_units.unwrap_or(0);
        let children = capacity.saturating_sub(subject);
        let subject_demand = pool.demand_units(ShareGroup::Subject);
        let children_demand = pool.demand_units(ShareGroup::Children);
        let current_subject = pool.entitled_units(ShareGroup::Subject);
        let current_children = pool.entitled_units(ShareGroup::Children);

        let uncovered = |demand: u64, entitled: u64| demand.saturating_sub(entitled);
        // Only the demand that can really be displaced is moved.
        let moved = |share: u64, current: u64, demand: u64| {
            share.min(demand).abs_diff(current.min(demand))
        };

        // Uncovered prefill/replay is the dominant cost: a side that cannot
        // keep its contexts resident pays to rebuild them.
        let mut cost = uncovered(subject_demand, subject) * capabilities.rebuild_cost_units
            + uncovered(children_demand, children) * capabilities.rebuild_cost_units;
        // Moving a share costs a transfer and is avoided unless it pays off.
        cost += moved(subject, current_subject, subject_demand) * capabilities.transfer_cost_units;
        cost += moved(children, current_children, children_demand) * capabilities.transfer_cost_units;
        cost
    }
}

impl KvPolicy for AutomaticKvPolicy {
    fn plan(&self, pool: &KvPoolState, capabilities: &KvCapabilities) -> AllocationPlan {
        let Some(capacity) = pool.capacity_units else {
            return AllocationPlan {
                subject_units: None,
                children_units: None,
                guarantee: RetentionGuarantee::Advisory,
                rationale:
                    "the residency pool capacity was not observed; no physical share is claimed"
                        .into(),
                candidates_evaluated: 0,
            };
        };
        let candidates = self.candidates(pool);
        if candidates.is_empty() {
            return AllocationPlan {
                subject_units: Some(capacity / 2),
                children_units: Some(capacity - capacity / 2),
                guarantee: RetentionGuarantee::Advisory,
                rationale: "no candidate could be derived from the current demand".into(),
                candidates_evaluated: 0,
            };
        }
        let subject_demand = pool.demand_units(ShareGroup::Subject);
        let children_demand = pool.demand_units(ShareGroup::Children);
        let total_demand = subject_demand + children_demand;
        let proportional = if total_demand > 0 {
            (capacity as u128 * subject_demand as u128 / total_demand as u128) as u64
        } else {
            capacity / 2
        };

        let mut best: Option<(u64, u64)> = None;
        let mut best_cost = u64::MAX;
        for (subject, children) in &candidates {
            let cost = self.cost(pool, capabilities, *subject);
            // Tie-break towards the demand proportional candidate: it is the
            // one that keeps following the workload.
            let tie = subject.abs_diff(proportional);
            let better = cost < best_cost
                || (cost == best_cost
                    && best.map(|(subject, _)| tie < subject.abs_diff(proportional)).unwrap_or(true));
            if better {
                best_cost = cost;
                best = Some((*subject, *children));
            }
        }
        let (subject, _) = best.unwrap_or((capacity / 2, capacity - capacity / 2));
        let (subject, held) = self.stabilise(capacity, subject);
        let children = capacity.saturating_sub(subject);
        let guarantee = if capabilities.hard_retention {
            RetentionGuarantee::Reserved
        } else {
            RetentionGuarantee::BestEffort
        };
        AllocationPlan {
            subject_units: Some(subject),
            children_units: Some(children),
            guarantee,
            rationale: if held {
                format!(
                    "demand subject={subject_demand} children={children_demand} capacity={capacity}; \
                     kept subject share {subject}/{capacity} inside the minimum hold and hysteresis \
                     band (best candidate cost {best_cost})"
                )
            } else {
                format!(
                    "demand subject={subject_demand} children={children_demand} capacity={capacity}; \
                     chosen subject share {subject}/{capacity} by weighted cost {best_cost}"
                )
            },
            candidates_evaluated: candidates.len(),
        }
    }
}

/// The wall clock in milliseconds. It ages a share hold and nothing else: it is
/// not a deadline and it never releases capacity.
fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Why a lease request could not be satisfied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KvError {
    #[error("the residency pool has no room for {required_units} units ({available_units} available)")]
    InsufficientCapacity { required_units: u64, available_units: u64 },
    #[error("no context lease exists for {0}")]
    UnknownLease(String),
    #[error("the context lease is {0:?}; it is not resident, so it cannot be read")]
    NotResident(LeaseState),
    #[error("no reader holds this context lease")]
    NoReaders,
    #[error(
        "the context still has an unresolved unknown read ({0}); its units are not free yet"
    )]
    UnresolvedUnknownRead(String),
}

/// The accounting outcome of releasing or closing a context.
///
/// `released` is true only when the lease is gone and its units are free. A
/// context still being read stays charged, so `readers` reports what has to
/// drain before the units can be reclaimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRelease {
    pub released: bool,
    pub readers: u32,
}

/// The outcome of one lease request.
#[derive(Debug, Clone, PartialEq)]
pub struct LeaseAdmission {
    pub lease: ContextLease,
    pub plan: AllocationPlan,
}

/// The allocator that turns a demand into a lease.
pub struct KvAllocator {
    policy: Box<dyn KvPolicy>,
    capabilities: parking_lot::RwLock<KvCapabilities>,
}

impl KvAllocator {
    pub fn new(policy: Box<dyn KvPolicy>, capabilities: KvCapabilities) -> Self {
        Self { policy, capabilities: parking_lot::RwLock::new(capabilities) }
    }

    pub fn capabilities(&self) -> KvCapabilities {
        *self.capabilities.read()
    }

    /// Adopt the residency controls one capability snapshot demonstrated.
    ///
    /// The pool mirrors the same set so diagnostics and the handoff record see
    /// what the allocator is actually relying on, including everything the
    /// backend did not demonstrate.
    pub fn apply_capabilities(&self, pool: &mut KvPoolState, capabilities: KvCapabilities) {
        *self.capabilities.write() = capabilities;
        pool.capabilities = capabilities;
    }

    /// Adopt the backend-level identity one probe established (SPEC §13.5).
    ///
    /// A value the probe did not establish never erases one that was already
    /// known, because an unknown is not evidence of a change. Every handle the
    /// merged identity no longer covers becomes invalid here, instead of being
    /// inherited by a rebuilt engine or a re-rendered template.
    pub fn adopt_identity(
        &self,
        pool: &mut KvPoolState,
        observed: &ResidencyIdentity,
    ) -> usize {
        pool.identity.adopt(observed);
        let identity = pool.identity.clone();
        pool.invalidate_stale(&identity)
    }

    /// Admit one context into the pool.
    pub fn acquire(
        &self,
        pool: &mut KvPoolState,
        demand: LeaseDemand,
    ) -> Result<LeaseAdmission, KvError> {
        let previous = pool.leases.remove(&demand.context_id);
        let mut lease = previous.clone().unwrap_or_else(|| {
            ContextLease::new(&demand.context_id, demand.side, demand.window_units)
        });
        lease.side = demand.side;
        lease.required_units = demand.window_units;
        lease.reusable_units = demand.reusable_units;
        lease.recompute_units = demand.recompute_units;
        lease.return_probability = demand.return_probability;
        lease.priority = demand.priority;
        lease.identity = demand.identity;
        lease.state = LeaseState::Resident;
        lease.retention.used_units = demand.window_units;
        lease.retention.reclaim_pending_units = 0;
        // A payload naming a residency option the backend never demonstrated
        // does not make the context resident. The gap is recorded on the lease
        // instead of being swallowed by an accepted HTTP response.
        let undemonstrated: Vec<&str> = demand
            .affinity_options
            .iter()
            .map(String::as_str)
            .filter(|option| {
                is_residency_option(*option) && !self.capabilities.read().demonstrates(*option)
            })
            .collect();
        lease.affinity_gap = (!undemonstrated.is_empty()).then(|| {
            format!(
                "the sealed request named {} but this backend never demonstrated the matching \
                 residency operation; the lease keeps only a soft claim",
                undemonstrated.join(", ")
            )
        });
        pool.leases.insert(demand.context_id.clone(), lease);

        let capabilities = self.capabilities();
        if let Some(capacity) = pool.capacity_units {
            // A context is only resident once it fits the observed pool. The
            // candidate's own units are taken out of the occupied set, so the
            // shortage is measured against the contexts that really hold KV.
            let incoming = demand.window_units;
            let others = pool.occupied_units().saturating_sub(incoming);
            let needed = others.saturating_add(incoming).saturating_sub(capacity);
            if needed > 0 {
                // The context does not fit. The least valuable idle contexts are
                // asked to release so a later request can fit; a requested
                // release is not free space, so this context is refused now
                // instead of being admitted over the observed pool.
                pool.leases.remove(&demand.context_id);
                self.reclaim(pool);
                // The refused context must not erase the accounting of the
                // context that was already there: restore it as it was and
                // re-plan, instead of silently over-committing.
                if let Some(previous) = previous {
                    pool.leases.insert(previous.context_id.clone(), previous);
                }
                let available_units = capacity.saturating_sub(pool.occupied_units());
                let plan = self.policy.plan(pool, &capabilities);
                self.apply_plan(pool, &plan);
                return Err(KvError::InsufficientCapacity {
                    required_units: incoming,
                    available_units,
                });
            }
        }
        let plan = self.policy.plan(pool, &capabilities);
        self.apply_plan(pool, &plan);
        Ok(LeaseAdmission {
            lease: pool.leases.get(&demand.context_id).cloned()
                .ok_or_else(|| KvError::UnknownLease(demand.context_id.clone()))?,
            plan,
        })
    }

    /// Register one reader of a resident context.
    ///
    /// While a reader holds the context its units stay charged and it is not an
    /// eviction candidate, so a concurrent admission cannot take the space a
    /// running generation is still reading (§13.3 step 6).
    ///
    /// A context whose release was already requested but never confirmed is
    /// read again: the pending release is revoked and the context returns to
    /// resident instead of its KV being freed underneath this read.
    pub fn begin_read(&self, pool: &mut KvPoolState, context_id: &str) -> Result<u32, KvError> {
        let lease = pool
            .leases
            .get_mut(context_id)
            .ok_or_else(|| KvError::UnknownLease(context_id.to_string()))?;
        match lease.state {
            LeaseState::ReclaimPending => {
                lease.state = LeaseState::Resident;
                lease.retention.reclaim_pending_units = 0;
            }
            LeaseState::Resident | LeaseState::Transferring => {}
            other => return Err(KvError::NotResident(other)),
        }
        lease.readers += 1;
        lease.lease_revision += 1;
        Ok(lease.readers)
    }

    /// End one reader's hold. A context that was waiting for release is only
    /// freed once its last reader finishes.
    pub fn end_read(&self, pool: &mut KvPoolState, context_id: &str) -> Result<LeaseRelease, KvError> {
        if pool
            .leases
            .get(context_id)
            .is_none_or(|lease| lease.readers == 0)
        {
            return Err(match pool.leases.contains_key(context_id) {
                true => KvError::NoReaders,
                false => KvError::UnknownLease(context_id.to_string()),
            });
        }
        self.drain_reader(pool, context_id)
    }

    /// Keep one context's read hold open because the request's outcome is
    /// unknown (SPEC §41.9, §41.10).
    ///
    /// A dropped connection, a deadline or a lost receipt does not prove the
    /// backend stopped reading the context, so its units stay charged and it
    /// stays off the eviction list until the original terminal evidence arrives.
    /// Only a context that already had a read hold can be kept: the pool never
    /// invents occupancy for a context nothing was observed about.
    pub fn hold_unknown_read(
        &self,
        pool: &mut KvPoolState,
        request_id: &str,
        context_id: &str,
    ) -> bool {
        if pool.unknown_reads.contains_key(request_id) {
            return false;
        }
        if !pool
            .leases
            .get(context_id)
            .is_some_and(|lease| lease.readers > 0)
        {
            return false;
        }
        pool.unknown_reads
            .insert(request_id.to_string(), context_id.to_string());
        true
    }

    /// Drain the read hold one reconciled request no longer justifies.
    ///
    /// The hold taken at dispatch is the reader this ends, so a context that
    /// was waiting for release is freed here if it was the last one. A request
    /// without a hold is left alone instead of decrementing some other reader.
    pub fn release_unknown_read(&self, pool: &mut KvPoolState, request_id: &str) -> bool {
        let Some(context_id) = pool.unknown_reads.remove(request_id) else {
            return false;
        };
        let _ = self.drain_reader(pool, &context_id);
        true
    }

    /// End one reader's hold on a context, freeing the record when it was the
    /// last reader of a context that had been asked to release or was
    /// invalidated. An invalidated handle whose last reader finishes is simply
    /// gone: it already stopped counting as residency when its identity changed.
    fn drain_reader(
        &self,
        pool: &mut KvPoolState,
        context_id: &str,
    ) -> Result<LeaseRelease, KvError> {
        let lease = pool
            .leases
            .get_mut(context_id)
            .ok_or_else(|| KvError::UnknownLease(context_id.to_string()))?;
        if lease.readers == 0 {
            return Err(KvError::NoReaders);
        }
        lease.readers -= 1;
        lease.lease_revision += 1;
        if lease.readers == 0
            && matches!(lease.state, LeaseState::ReclaimPending | LeaseState::Invalid)
        {
            pool.leases.remove(context_id);
            return Ok(LeaseRelease { released: true, readers: 0 });
        }
        Ok(LeaseRelease { released: false, readers: lease.readers })
    }

    /// Release one context from the pool.
    ///
    /// A context whose readers are still draining is moved to reclaim-pending
    /// and keeps its units charged; only the last reader frees them.
    pub fn release(&self, pool: &mut KvPoolState, context_id: &str) -> Result<LeaseRelease, KvError> {
        if !pool.leases.contains_key(context_id) {
            return Err(KvError::UnknownLease(context_id.to_string()));
        }
        // An outcome that was never reconciled may still be reading this
        // context, so a release cannot confirm the KV is free.
        if pool.has_unknown_read(context_id) {
            return Err(KvError::UnresolvedUnknownRead(context_id.to_string()));
        }
        let readers = {
            let lease = pool
                .leases
                .get_mut(context_id)
                .ok_or_else(|| KvError::UnknownLease(context_id.to_string()))?;
            if lease.readers > 0 {
                lease.state = LeaseState::ReclaimPending;
                lease.retention.reclaim_pending_units = lease.required_units;
                lease.lease_revision += 1;
            }
            lease.readers
        };
        if readers > 0 {
            return Ok(LeaseRelease { released: false, readers });
        }
        pool.leases.remove(context_id);
        Ok(LeaseRelease { released: true, readers: 0 })
    }

    /// Recompute the share and move leases between resident and reclaiming
    /// states. The order is reserve, transfer, confirm, publish/release: a
    /// destination is only published after the source release is confirmed.
    ///
    /// The plan is written into the leases first, then the pool enforces its
    /// capacity: a pool whose observed size shrank reclaims down to the new
    /// figure here instead of staying over capacity until the next admission.
    /// A context still being read is never an eviction candidate.
    pub fn rebalance(&self, pool: &mut KvPoolState) -> Result<AllocationPlan, KvError> {
        let capabilities = self.capabilities();
        let plan = self.policy.plan(pool, &capabilities);
        self.apply_plan(pool, &plan);
        self.reclaim(pool);
        Ok(plan)
    }

    /// Write the plan into the leases: entitlement per side, borrow above the
    /// entitlement and reclaim-pending units.
    fn apply_plan(&self, pool: &mut KvPoolState, plan: &AllocationPlan) {
        let (Some(subject_share), Some(children_share)) = (plan.subject_units, plan.children_units)
        else {
            return;
        };
        let subject_demand = pool.demand_units(ShareGroup::Subject);
        let children_demand = pool.demand_units(ShareGroup::Children);
        for lease in pool.leases.values_mut() {
            let (share, demand) = match lease.side.share_group() {
                ShareGroup::Subject => (subject_share, subject_demand),
                ShareGroup::Children => (children_share, children_demand),
            };
            // The side's share is divided between its contexts by demand.
            let entitled = if demand == 0 {
                0
            } else {
                share.saturating_mul(lease.required_units) / demand
            };
            lease.retention.entitled_units = entitled;
            // A context whose sealed request named an option the backend never
            // demonstrated keeps only a soft claim, whatever the plan says.
            lease.retention.guarantee = if lease.affinity_gap.is_some() {
                RetentionGuarantee::Advisory
            } else {
                plan.guarantee
            };
            lease.retention.used_units = if lease.state == LeaseState::Resident {
                lease.required_units
            } else {
                0
            };
            lease.retention.borrowed_units = lease
                .retention
                .used_units
                .saturating_sub(lease.retention.entitled_units);
        }
    }

    /// Request the release of the least valuable contexts until the excess is
    /// covered. The order is by return probability and priority, so a hot
    /// context is not evicted to keep a cold one, and only as many contexts as
    /// the excess needs are asked to release.
    ///
    /// A context whose reader is still draining is not an eviction candidate:
    /// its units are in use until the read finishes. Neither is the context an
    /// admission is currently deciding on — a refused admission takes its own
    /// lease out of the pool before asking for releases — nor one an unresolved
    /// unknown outcome may still be reading: that hold only ends with its
    /// terminal evidence.
    ///
    /// A requested release is not free space: reclaim-pending units stay
    /// occupied until the backend confirms the release. The excess is therefore
    /// compared against the units already pending plus the ones requested in
    /// this pass, so a second pass does not evict the same pool again.
    fn reclaim(&self, pool: &mut KvPoolState) -> bool {
        let Some(capacity) = pool.capacity_units else { return false };
        let mut requested = pool.reclaim_pending_units();
        let mut reclaimed = false;
        loop {
            if pool.occupied_units().saturating_sub(requested) <= capacity {
                break;
            }
            let Some(candidate) = pool
                .leases
                .values()
                .filter(|lease| {
                    lease.state == LeaseState::Resident
                        && lease.readers == 0
                        && !pool.has_unknown_read(&lease.context_id)
                })
                .min_by(|left, right| {
                    left.return_probability
                        .partial_cmp(&right.return_probability)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(left.priority.cmp(&right.priority))
                        .then(left.recompute_units.cmp(&right.recompute_units))
                })
                .map(|lease| lease.context_id.clone())
            else {
                break;
            };
            if let Some(lease) = pool.leases.get_mut(&candidate) {
                lease.state = LeaseState::ReclaimPending;
                lease.retention.reclaim_pending_units = lease.required_units;
                lease.retention.used_units = 0;
                lease.lease_revision += 1;
                requested = requested.saturating_add(lease.required_units);
            }
            reclaimed = true;
        }
        reclaimed
    }
}
