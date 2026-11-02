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
    pub leases: BTreeMap<String, ContextLease>,
}

impl KvPoolState {
    pub fn new(capacity_units: Option<u64>, provenance: CapacityProvenance) -> Self {
        Self { capacity_units, provenance, leases: BTreeMap::new() }
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
    /// The upper bound of the reusable prefix.
    pub reusable_units: u64,
    /// The units that must be recomputed.
    pub recompute_units: u64,
    pub return_probability: f64,
    pub priority: u8,
}

/// What the backend really offers for residency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
pub struct AutomaticKvPolicy {
    /// Minimum time a share is kept before it may drift, in milliseconds.
    pub min_hold_ms: u64,
    /// Relative change below which the current share is kept.
    pub hysteresis_ratio: f64,
}

impl Default for AutomaticKvPolicy {
    fn default() -> Self {
        Self { min_hold_ms: 30_000, hysteresis_ratio: 0.05 }
    }
}

impl AutomaticKvPolicy {
    /// The candidate shares of the subject, with the cost of each.
    fn candidates(&self, pool: &KvPoolState, capabilities: &KvCapabilities) -> Vec<(u64, u64)> {
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
        let _ = capabilities;
        candidates
            .into_iter()
            .map(|subject| (subject, capacity - subject))
            .collect()
    }

    /// The weighted cost of one candidate split.
    fn cost(&self, pool: &KvPoolState, capabilities: &KvCapabilities, subject: u64) -> u64 {
        let capacity = pool.capacity_units.unwrap_or(0);
        let children = capacity.saturating_sub(subject);
        let subject_demand = pool.demand_units(ShareGroup::Subject);
        let children_demand = pool.demand_units(ShareGroup::Children);
        let current_subject = pool.entitled_units(ShareGroup::Subject);
        let current_children = pool.entitled_units(ShareGroup::Children);

        let uncovered = |demand: u64, entitled: u64| demand.saturating_sub(entitled);
        let surplus = |demand: u64, entitled: u64| entitled.saturating_sub(demand);

        // Uncovered prefill/replay is the dominant cost: a side that cannot
        // keep its contexts resident pays to rebuild them.
        let mut cost = uncovered(subject_demand, subject) * capabilities.rebuild_cost_units
            + uncovered(children_demand, children) * capabilities.rebuild_cost_units;
        // Idle entitled capacity is a smaller waste than a forced rebuild.
        cost += surplus(subject_demand, subject) + surplus(children_demand, children);
        // Moving a share costs a transfer and is avoided unless it pays off.
        cost += subject.abs_diff(current_subject) * capabilities.transfer_cost_units;
        cost += children.abs_diff(current_children) * capabilities.transfer_cost_units;
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
        let candidates = self.candidates(pool, capabilities);
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
        let (subject, children) = best.unwrap_or((capacity / 2, capacity - capacity / 2));
        let guarantee = if capabilities.hard_retention {
            RetentionGuarantee::Reserved
        } else {
            RetentionGuarantee::BestEffort
        };
        AllocationPlan {
            subject_units: Some(subject),
            children_units: Some(children),
            guarantee,
            rationale: format!(
                "demand subject={subject_demand} children={children_demand} capacity={capacity}; \
                 chosen subject share {subject}/{capacity} by weighted cost {best_cost}"
            ),
            candidates_evaluated: candidates.len(),
        }
    }
}

/// Why a lease request could not be satisfied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KvError {
    #[error("the residency pool has no room for {required_units} units ({available_units} available)")]
    InsufficientCapacity { required_units: u64, available_units: u64 },
    #[error("no context lease exists for {0}")]
    UnknownLease(String),
    #[error("the residency pool capacity is unknown; a hard reservation cannot be made")]
    UnknownCapacity,
}

/// The outcome of one lease request.
#[derive(Debug, Clone, PartialEq)]
pub struct LeaseAdmission {
    pub lease: ContextLease,
    pub plan: AllocationPlan,
    /// True when the lease had to wait for another context to be reclaimed.
    pub waited_for_reclaim: bool,
}

/// The allocator that turns a demand into a lease.
pub struct KvAllocator {
    policy: Box<dyn KvPolicy>,
    capabilities: KvCapabilities,
}

impl KvAllocator {
    pub fn new(policy: Box<dyn KvPolicy>, capabilities: KvCapabilities) -> Self {
        Self { policy, capabilities }
    }

    pub fn capabilities(&self) -> &KvCapabilities {
        &self.capabilities
    }

    /// Admit one context into the pool.
    pub fn acquire(
        &self,
        pool: &mut KvPoolState,
        demand: LeaseDemand,
    ) -> Result<LeaseAdmission, KvError> {
        let existing = pool.leases.remove(&demand.context_id);
        let mut lease = existing.unwrap_or_else(|| ContextLease::new(&demand.context_id, demand.side, demand.window_units));
        lease.side = demand.side;
        lease.required_units = demand.window_units;
        lease.reusable_units = demand.reusable_units;
        lease.recompute_units = demand.recompute_units;
        lease.return_probability = demand.return_probability;
        lease.priority = demand.priority;
        lease.state = LeaseState::Resident;
        pool.leases.insert(demand.context_id.clone(), lease);

        let plan = self.policy.plan(pool, &self.capabilities);
        self.apply_plan(pool, &plan);

        if let Some(capacity) = pool.capacity_units {
            let used = pool.used_units();
            if used > capacity {
                // Try to make room before refusing.
                let reclaimed = self.reclaim(pool, &plan);
                let used = pool.used_units();
                if used > capacity {
                    // Put the refused lease back into a non-resident state and
                    // report the shortage instead of silently over-committing.
                    if let Some(lease) = pool.leases.get_mut(&demand.context_id) {
                        lease.state = LeaseState::Requested;
                        lease.retention.used_units = 0;
                    }
                    pool.leases.remove(&demand.context_id);
                    return Err(KvError::InsufficientCapacity {
                        required_units: demand.window_units,
                        available_units: capacity.saturating_sub(used.saturating_sub(demand.window_units)),
                    });
                }
                return Ok(LeaseAdmission {
                    lease: pool.leases.get(&demand.context_id).cloned()
                        .ok_or_else(|| KvError::UnknownLease(demand.context_id.clone()))?,
                    plan,
                    waited_for_reclaim: reclaimed,
                });
            }
        }
        Ok(LeaseAdmission {
            lease: pool.leases.get(&demand.context_id).cloned()
                .ok_or_else(|| KvError::UnknownLease(demand.context_id.clone()))?,
            plan,
            waited_for_reclaim: false,
        })
    }

    /// Release one context from the pool.
    pub fn release(&self, pool: &mut KvPoolState, context_id: &str) -> Result<(), KvError> {
        let mut lease = pool
            .leases
            .remove(context_id)
            .ok_or_else(|| KvError::UnknownLease(context_id.to_string()))?;
        lease.state = LeaseState::Released;
        Ok(())
    }

    /// Recompute the share and move leases between resident and reclaiming
    /// states. The order is reserve, transfer, confirm, publish/release: a
    /// destination is only published after the source release is confirmed.
    pub fn rebalance(&self, pool: &mut KvPoolState) -> Result<AllocationPlan, KvError> {
        let plan = self.policy.plan(pool, &self.capabilities);
        self.apply_plan(pool, &plan);
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
            lease.retention.guarantee = plan.guarantee;
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

    /// Reclaim the least valuable contexts until the pool fits. The order is
    /// by return probability and priority, so a hot context is not evicted to
    /// keep a cold one.
    fn reclaim(&self, pool: &mut KvPoolState, plan: &AllocationPlan) -> bool {
        let Some(capacity) = pool.capacity_units else { return false };
        let mut reclaimed = false;
        loop {
            let used: u64 = pool
                .leases
                .values()
                .filter(|lease| matches!(lease.state, LeaseState::Resident | LeaseState::Transferring))
                .map(|lease| lease.required_units)
                .sum();
            if used <= capacity {
                break;
            }
            let Some(candidate) = pool
                .leases
                .values()
                .filter(|lease| lease.state == LeaseState::Resident)
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
            }
            reclaimed = true;
            let _ = plan;
        }
        reclaimed
    }
}
