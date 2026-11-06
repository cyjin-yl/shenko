//! Replaceable compute scheduling policy — SPEC §13.1–13.4, §22.1, §41.9.
//!
//! Compute admission is a policy plugin, symmetric with the residency
//! `crate::kv::KvPolicy`: the kernel must not hard-code one share of the
//! single physical compute permit. This module owns the decision "which
//! runnable request runs next" and nothing else — it never allocates KV, never
//! sends, never drops and never fails a request.
//!
//! Three fairness properties are structural rather than heuristic:
//!
//! * **Per-lineage rotation.** Every tier queues per durable line
//!   (`FairnessScope::lineage`) and serves those lines round-robin, so a
//!   parent that keeps creating descendants (every new task is its own agent)
//!   cannot buy extra turns: the whole line keeps one share, and descendants
//!   take turns inside it.
//! * **Bounded same-side service.** SPEC §41.9: while both sides stay runnable
//!   and no higher class event is ready, one side never runs more than
//!   `AutomaticBrokerPolicy::max_consecutive_side` complete generations in a
//!   row. This is what actually stops a deep subject backlog from starving a
//!   ready child — the latency comparison alone would keep serving the older
//!   backlog.
//! * **Bounded waiting.** A tier whose oldest queued request has waited past
//!   the policy deadline runs next regardless of the comparison above.
//!
//! The default policy balances the subject side against the children side by
//! *observed* queue latency (the oldest ready request on each side), not by a
//! fixed ratio: symmetric offer and symmetric demand tie on every decision and
//! therefore alternate, while one side falling behind takes the next turns
//! until the latencies meet again. Nothing here pins `0.5` and no per-tier
//! quota is exposed as configuration, so the feasible 50/50 of §13.2 remains an
//! outcome of the workload rather than an input.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use serde::Serialize;
use shenko_types::ids::EntityId;

use crate::request::InferencePriority;

/// Number of dispatch tiers. It matches the numeric order of
/// `InferencePriority` so a tier index is never translated through a table
/// that could drift from the enum.
pub const TIERS: usize = 4;

/// One dispatch tier. Tier order is the priority order, which is also the
/// order the broker documents to callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Low,
    Normal,
    Subject,
    Urgent,
}

impl Tier {
    pub const fn of(priority: InferencePriority) -> Self {
        match priority {
            InferencePriority::Low => Self::Low,
            InferencePriority::Normal => Self::Normal,
            InferencePriority::Subject => Self::Subject,
            InferencePriority::Urgent => Self::Urgent,
        }
    }

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn priority(self) -> InferencePriority {
        match self {
            Self::Low => InferencePriority::Low,
            Self::Normal => InferencePriority::Normal,
            Self::Subject => InferencePriority::Subject,
            Self::Urgent => InferencePriority::Urgent,
        }
    }

    /// The aggregate side of SPEC §13.2 this tier is charged to.
    pub const fn side(self) -> Side {
        match self {
            Self::Subject => Side::Main,
            Self::Normal | Self::Low => Side::Child,
            Self::Urgent => Side::External,
        }
    }
}

/// The aggregate sides the scheduler balances. Urgent work is external to the
/// subject/children split: it is an interruption the subject handles, so it is
/// billed to neither side's share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Main,
    Child,
    External,
}

impl Side {
    pub const fn tiers(self) -> &'static [Tier] {
        match self {
            Self::Main => &[Tier::Subject],
            Self::Child => &[Tier::Normal, Tier::Low],
            Self::External => &[Tier::Urgent],
        }
    }

    /// The other side of the subject/children split. External work has no
    /// counterpart because it is not part of that split.
    pub const fn opposite(self) -> Option<Self> {
        match self {
            Self::Main => Some(Self::Child),
            Self::Child => Some(Self::Main),
            Self::External => None,
        }
    }
}

/// The fairness identity of one queued request.
///
/// `lineage` is the durable line the request descends from — the conversation
/// or delegation root the broker derives it from — and owns the rotation turn.
/// `agent` only orders work *inside* one line. Splitting the two is what makes
/// recursive fan-out free: creating a descendant task adds an agent to the line
/// but never a second turn against other lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FairnessScope {
    pub lineage: EntityId,
    pub agent: EntityId,
}

impl FairnessScope {
    pub const fn new(lineage: EntityId, agent: EntityId) -> Self {
        Self { lineage, agent }
    }

    /// A scope whose line is one agent. Callers that cannot name a wider line
    /// fall back to the agent itself, never to a shared global line.
    pub const fn per_agent(agent: EntityId) -> Self {
        Self { lineage: agent, agent }
    }
}

/// The readiness view a policy decides over. It carries queue latency only:
/// no prompt content, no provider identity, no request bodies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueuePressure {
    /// Ready requests per tier, indexed by `Tier::index`.
    pub ready: [usize; TIERS],
    /// Ready lines per tier. A tier with ten queued requests on one line has
    /// one turn to spend, not ten.
    pub lineages: [usize; TIERS],
    /// Waiting time of the oldest ready request in the tier, in ms.
    pub oldest_ms: [u64; TIERS],
}

impl QueuePressure {
    pub fn any_ready(&self) -> bool {
        self.ready.iter().any(|count| *count > 0)
    }

    pub fn ready(&self, tier: Tier) -> usize {
        self.ready[tier.index()]
    }

    pub fn lineages(&self, tier: Tier) -> usize {
        self.lineages[tier.index()]
    }

    pub fn has_side(&self, side: Side) -> bool {
        side.tiers().iter().any(|tier| self.ready(*tier) > 0)
    }

    pub fn oldest(&self, tier: Tier) -> u64 {
        self.oldest_ms[tier.index()]
    }

    /// The longest-waiting ready request charged to one side. `None` when the
    /// side has no ready work.
    pub fn side_oldest(&self, side: Side) -> Option<u64> {
        side.tiers()
            .iter()
            .filter(|tier| self.ready(**tier) > 0)
            .map(|tier| self.oldest(*tier))
            .max()
    }

    /// The longest-waiting ready tier. Deterministic fallback when a policy
    /// declines to choose: queued work is never stalled by a policy.
    pub fn longest_waiting(&self) -> Option<Tier> {
        let mut best: Option<Tier> = None;
        for tier in [Tier::Urgent, Tier::Subject, Tier::Normal, Tier::Low] {
            if self.ready(tier) == 0 {
                continue;
            }
            match best {
                None => best = Some(tier),
                Some(current) if self.oldest(tier) > self.oldest(current) => best = Some(tier),
                Some(_) => {}
            }
        }
        best
    }
}

/// The policy's own tie-break and service state. It belongs to the queue, not
/// to the policy instance, so one installed policy can serve any number of
/// dispatch loops without sharing mutable state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyCursor {
    /// Side to serve when both sides carry identical waiting pressure.
    pub preferred_side: Side,
    /// Child tier to serve when both child tiers carry identical pressure.
    pub preferred_child: Tier,
    /// Consecutive urgent dispatches since ordinary work last ran.
    pub urgent_streak: u32,
    /// Last side of the subject/children split that was served, if any.
    pub longest_side: Option<Side>,
    /// How many consecutive dispatches that side has had.
    pub consecutive_side: u32,
    /// Dispatches recorded, including policy-declined fallbacks.
    pub decisions: u64,
}

impl Default for PolicyCursor {
    fn default() -> Self {
        Self {
            preferred_side: Side::Main,
            preferred_child: Tier::Normal,
            urgent_streak: 0,
            longest_side: None,
            consecutive_side: 0,
            decisions: 0,
        }
    }
}

impl PolicyCursor {
    /// Record the tier that was actually dispatched. The queue is the single
    /// authority for this call: a policy that declines its turn still moves the
    /// fairness state, so a fallback cannot silently reset the bounds.
    pub fn record(&mut self, tier: Tier) {
        match tier.side() {
            Side::Main => {
                self.preferred_side = Side::Child;
                self.urgent_streak = 0;
            }
            Side::Child => {
                self.preferred_side = Side::Main;
                self.urgent_streak = 0;
                self.preferred_child = match tier {
                    Tier::Normal => Tier::Low,
                    _ => Tier::Normal,
                };
            }
            Side::External => self.urgent_streak = self.urgent_streak.saturating_add(1),
        }
        let side = tier.side();
        // An interruption is not service to either balanced side: it neither
        // continues nor breaks their consecutive run.
        if side != Side::External {
            if self.longest_side == Some(side) {
                self.consecutive_side = self.consecutive_side.saturating_add(1);
            } else {
                self.longest_side = Some(side);
                self.consecutive_side = 1;
            }
        }
        self.decisions = self.decisions.saturating_add(1);
    }

    /// Dispatches this side has had in a row, right now.
    pub fn consecutive_for(&self, side: Side) -> u32 {
        if self.longest_side == Some(side) {
            self.consecutive_side
        } else {
            0
        }
    }
}

/// A replaceable compute scheduling policy.
///
/// `choose` returns the tier to dispatch next, or `None` to leave the decision
/// to the queue's deterministic longest-waiting fallback. Declining never
/// drops, fails or indefinitely defers queued work. The queue records the
/// served tier into the cursor afterwards, so a policy must not record the
/// dispatch itself.
pub trait BrokerPolicy: Send + Sync {
    /// Stable identity for evidence and diagnostics.
    fn name(&self) -> &'static str;

    fn choose(&self, pressure: &QueuePressure, cursor: &mut PolicyCursor) -> Option<Tier>;
}

/// The default automatic policy.
///
/// It compares the *observed* queue latency of the subject side against the
/// children side. On a symmetric offer the two sides tie and alternate; when
/// one side falls behind it takes the next turns until the latencies meet
/// again, and when the workload changes the order follows it. There is no
/// configured share and no `0.5` constant.
pub struct AutomaticBrokerPolicy {
    /// Largest number of consecutive urgent dispatches before ordinary work
    /// must get its turn. `0` makes urgent wait behind any ordinary work.
    pub max_urgent_streak: u32,
    /// Largest number of consecutive complete generations one side of the
    /// subject/children split may run while the other side is ready. SPEC
    /// §41.9 fixes the default at two; `0` disables the bound.
    pub max_consecutive_side: u32,
    /// Waiting time after which the longest-waiting ready tier runs next even
    /// if the comparison prefers another tier. `0` disables the bound; it is a
    /// service guarantee, not a latency target.
    pub starvation_deadline_ms: u64,
}

impl Default for AutomaticBrokerPolicy {
    fn default() -> Self {
        Self {
            max_urgent_streak: 1,
            max_consecutive_side: 2,
            starvation_deadline_ms: 30_000,
        }
    }
}

impl AutomaticBrokerPolicy {
    /// The longest-waiting tier that already passed the service deadline.
    /// Ties keep the higher tier, so urgency still wins inside the bound.
    fn starving_tier(&self, pressure: &QueuePressure) -> Option<Tier> {
        if self.starvation_deadline_ms == 0 {
            return None;
        }
        let mut best: Option<Tier> = None;
        for tier in [Tier::Urgent, Tier::Subject, Tier::Normal, Tier::Low] {
            if pressure.ready(tier) == 0 {
                continue;
            }
            let waited = pressure.oldest(tier);
            if waited < self.starvation_deadline_ms {
                continue;
            }
            match best {
                None => best = Some(tier),
                Some(current) if waited > pressure.oldest(current) => best = Some(tier),
                Some(_) => {}
            }
        }
        best
    }

    /// The tier inside one side that should run next.
    fn tier_for_side(pressure: &QueuePressure, cursor: &PolicyCursor, side: Side) -> Option<Tier> {
        match side {
            Side::Main => (pressure.ready(Tier::Subject) > 0).then_some(Tier::Subject),
            Side::Child => {
                let normal_ready = pressure.ready(Tier::Normal) > 0;
                let low_ready = pressure.ready(Tier::Low) > 0;
                match (normal_ready, low_ready) {
                    (true, false) => Some(Tier::Normal),
                    (false, true) => Some(Tier::Low),
                    (true, true) => {
                        let normal_wait = pressure.oldest(Tier::Normal);
                        let low_wait = pressure.oldest(Tier::Low);
                        match normal_wait.cmp(&low_wait) {
                            std::cmp::Ordering::Greater => Some(Tier::Normal),
                            std::cmp::Ordering::Less => Some(Tier::Low),
                            std::cmp::Ordering::Equal => match cursor.preferred_child {
                                Tier::Low => Some(Tier::Low),
                                _ => Some(Tier::Normal),
                            },
                        }
                    }
                    (false, false) => None,
                }
            }
            Side::External => (pressure.ready(Tier::Urgent) > 0).then_some(Tier::Urgent),
        }
    }
}

impl BrokerPolicy for AutomaticBrokerPolicy {
    fn name(&self) -> &'static str {
        "automatic.pressure-balanced"
    }

    fn choose(&self, pressure: &QueuePressure, cursor: &mut PolicyCursor) -> Option<Tier> {
        if !pressure.any_ready() {
            return None;
        }
        // 1. Bounded service: an already-starved tier runs next, whatever the
        // pressure comparison would prefer.
        if let Some(tier) = self.starving_tier(pressure) {
            return Some(tier);
        }
        // 2. Urgency quota: urgent may open a turn, but consecutive urgent
        // dispatches are bounded, so ordinary work cannot be withheld.
        if pressure.ready(Tier::Urgent) > 0 && cursor.urgent_streak < self.max_urgent_streak {
            return Some(Tier::Urgent);
        }
        // 3. Subject side against the children side, by observed latency: the
        // side whose oldest request has waited longer runs next, and equal
        // latency alternates.
        let main = pressure.has_side(Side::Main);
        let child = pressure.has_side(Side::Child);
        let mut side = match (main, child) {
            (true, true) => {
                let main_wait = pressure.side_oldest(Side::Main).unwrap_or(0);
                let child_wait = pressure.side_oldest(Side::Child).unwrap_or(0);
                match main_wait.cmp(&child_wait) {
                    std::cmp::Ordering::Greater => Side::Main,
                    std::cmp::Ordering::Less => Side::Child,
                    std::cmp::Ordering::Equal => match cursor.preferred_side {
                        Side::Child => Side::Child,
                        _ => Side::Main,
                    },
                }
            }
            (true, false) => Side::Main,
            (false, true) => Side::Child,
            (false, false) => return None,
        };
        // 4. Consecutive same-side service is bounded, so a long backlog on one
        // side cannot hold the permit for as long as its oldest request stays
        // older than the other side's.
        if self.max_consecutive_side > 0 {
            if let Some(other) = side.opposite() {
                if cursor.consecutive_for(side) >= self.max_consecutive_side
                    && pressure.has_side(other)
                {
                    side = other;
                }
            }
        }
        Self::tier_for_side(pressure, cursor, side)
    }
}

/// The scheduler-relevant facts of one submission. The sealed request and the
/// provider-facing priority are untouched: only the queue sees these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchFacts {
    /// Identity of the queued request, used to find it for cancellation.
    pub key: EntityId,
    /// The priority sealed into the request.
    pub priority: InferencePriority,
    /// The fairness line and agent.
    pub scope: FairnessScope,
    /// Class of the durable task parked on this result, if one is waiting.
    /// Dependency inheritance: the queue sorts the request by the *effective*
    /// tier, which is the higher of its own and the inherited class.
    pub inherited: Option<InferencePriority>,
}

impl DispatchFacts {
    /// The tier the queue actually files this request under.
    ///
    /// `Urgent` is the external interruption tier and cannot be borrowed by an
    /// inherited class: a parked task is an ordinary participant, so
    /// inheritance is clamped to the subject tier.
    pub fn effective_tier(&self) -> Tier {
        let inherited = self
            .inherited
            .map(Tier::of)
            .map(|tier| tier.min(Tier::Subject));
        match inherited {
            Some(inherited) => Tier::of(self.priority).max(inherited),
            None => Tier::of(self.priority),
        }
    }

    /// Whether the effective tier came from inheritance rather than the sealed
    /// priority; the fairness counters report these separately.
    pub fn is_promoted(&self) -> bool {
        self.effective_tier() != Tier::of(self.priority)
    }
}

struct Queued<T> {
    key: EntityId,
    scope: FairnessScope,
    promoted: bool,
    enqueued_at_ms: i64,
    payload: T,
}

/// One line inside a tier: agents served round-robin, FIFO per agent.
struct LineageQueue<T> {
    agents: HashMap<EntityId, VecDeque<Queued<T>>>,
    /// Agents of this line with at least one queued request, in service order.
    /// It holds no duplicates and never names an empty queue.
    rotation: VecDeque<EntityId>,
    len: usize,
    oldest_enqueued_ms: Option<i64>,
}

impl<T> LineageQueue<T> {
    fn new() -> Self {
        Self {
            agents: HashMap::new(),
            rotation: VecDeque::new(),
            len: 0,
            oldest_enqueued_ms: None,
        }
    }

    fn push(&mut self, queued: Queued<T>) {
        self.len += 1;
        self.oldest_enqueued_ms = Some(
            self.oldest_enqueued_ms
                .map_or(queued.enqueued_at_ms, |oldest| oldest.min(queued.enqueued_at_ms)),
        );
        let agent = queued.scope.agent;
        if let Some(queue) = self.agents.get_mut(&agent) {
            queue.push_back(queued);
            return;
        }
        self.agents.insert(agent, VecDeque::from([queued]));
        self.rotation.push_back(agent);
    }

    fn pop_one(&mut self) -> Option<Queued<T>> {
        let agent = *self.rotation.front()?;
        let queue = self.agents.get_mut(&agent)?;
        let queued = queue.pop_front()?;
        let drained = queue.is_empty();
        self.rotation.pop_front();
        if drained {
            self.agents.remove(&agent);
        } else {
            self.rotation.push_back(agent);
        }
        self.len -= 1;
        if self.oldest_enqueued_ms == Some(queued.enqueued_at_ms) {
            self.oldest_enqueued_ms = self
                .agents
                .values()
                .filter_map(|queue| queue.front())
                .map(|queued| queued.enqueued_at_ms)
                .min();
        }
        Some(queued)
    }
}

/// One tier: per-line rotation, and per-agent rotation inside a line.
struct TierQueue<T> {
    lineages: HashMap<EntityId, LineageQueue<T>>,
    /// Lines of this tier with at least one queued request, in service order.
    rotation: VecDeque<EntityId>,
    len: usize,
    oldest_enqueued_ms: Option<i64>,
}

impl<T> TierQueue<T> {
    fn new() -> Self {
        Self {
            lineages: HashMap::new(),
            rotation: VecDeque::new(),
            len: 0,
            oldest_enqueued_ms: None,
        }
    }

    fn push(&mut self, queued: Queued<T>) {
        self.len += 1;
        self.oldest_enqueued_ms = Some(
            self.oldest_enqueued_ms
                .map_or(queued.enqueued_at_ms, |oldest| oldest.min(queued.enqueued_at_ms)),
        );
        let lineage = queued.scope.lineage;
        if let Some(queue) = self.lineages.get_mut(&lineage) {
            queue.push(queued);
            return;
        }
        let mut queue = LineageQueue::new();
        queue.push(queued);
        self.lineages.insert(lineage, queue);
        self.rotation.push_back(lineage);
    }

    fn pop_one(&mut self) -> Option<Queued<T>> {
        let lineage = *self.rotation.front()?;
        let queue = self.lineages.get_mut(&lineage)?;
        let queued = queue.pop_one()?;
        let drained = queue.len == 0;
        self.rotation.pop_front();
        if drained {
            self.lineages.remove(&lineage);
        } else {
            self.rotation.push_back(lineage);
        }
        self.len -= 1;
        if self.oldest_enqueued_ms == Some(queued.enqueued_at_ms) {
            self.oldest_enqueued_ms = self
                .lineages
                .values()
                .filter_map(|queue| queue.oldest_enqueued_ms)
                .min();
        }
        Some(queued)
    }

    fn oldest_wait(&self, now_ms: i64) -> u64 {
        self.oldest_enqueued_ms
            .map_or(0, |oldest| (now_ms - oldest).max(0) as u64)
    }

    /// Remove one still-waiting request by identity and return its payload.
    ///
    /// A request that already left this tier is not found: the caller must use
    /// the running-request path instead of pretending it was cancelled here.
    fn remove(&mut self, key: EntityId) -> Option<T> {
        let mut located = None;
        for (lineage, queue) in self.lineages.iter() {
            for (agent, calls) in queue.agents.iter() {
                if calls.iter().any(|queued| queued.key == key) {
                    located = Some((*lineage, *agent));
                    break;
                }
            }
            if located.is_some() {
                break;
            }
        }
        let (lineage, agent) = located?;
        let queue = self.lineages.get_mut(&lineage)?;
        let calls = queue.agents.get_mut(&agent)?;
        let position = calls.iter().position(|queued| queued.key == key)?;
        let queued = calls.remove(position)?;
        queue.len -= 1;
        if calls.is_empty() {
            queue.agents.remove(&agent);
            queue.rotation.retain(|candidate| *candidate != agent);
        }
        queue.oldest_enqueued_ms = queue
            .agents
            .values()
            .filter_map(|calls| calls.front())
            .map(|queued| queued.enqueued_at_ms)
            .min();
        if queue.len == 0 {
            self.lineages.remove(&lineage);
            self.rotation.retain(|candidate| *candidate != lineage);
        }
        self.len -= 1;
        self.oldest_enqueued_ms = self
            .lineages
            .values()
            .filter_map(|queue| queue.oldest_enqueued_ms)
            .min();
        Some(queued.payload)
    }
}

/// One dispatch decision.
pub struct Dispatched<T> {
    pub payload: T,
    pub tier: Tier,
    /// Time the request spent queued before this dispatch.
    pub waited_ms: u64,
}

/// The fairness state an operator or the subject can read back: the SPEC §13.6
/// minimum diagnostics for queue wait, the subject's maximum wait and child
/// fairness. It carries no prompt content and no provider identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FairnessCounters {
    pub queued: [usize; TIERS],
    pub lineages: [usize; TIERS],
    /// Enqueue time of the oldest queued request per tier. The wait is derived
    /// when a reader asks, so a snapshot never reports a stale age for work
    /// that is still queued.
    pub oldest_enqueued_ms: [Option<i64>; TIERS],
    pub served: [u64; TIERS],
    pub max_wait_ms: [u64; TIERS],
    /// Dispatches whose tier came from dependency inheritance.
    pub promoted: u64,
    pub longest_side: Option<Side>,
    pub consecutive_side: u32,
    pub decisions: u64,
}

/// The counters plus the identity of the policy that produced them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FairnessSnapshot {
    pub policy: &'static str,
    pub queued: [usize; TIERS],
    pub lineages: [usize; TIERS],
    pub oldest_wait_ms: [u64; TIERS],
    pub served: [u64; TIERS],
    pub max_wait_ms: [u64; TIERS],
    /// Dispatches whose tier came from dependency inheritance.
    pub promoted: u64,
    pub longest_side: Option<Side>,
    pub consecutive_side: u32,
    pub decisions: u64,
}

impl FairnessCounters {
    pub fn snapshot(self, policy: &'static str, now_ms: i64) -> FairnessSnapshot {
        let oldest_wait_ms = self
            .oldest_enqueued_ms
            .map(|oldest| oldest.map_or(0, |oldest| (now_ms - oldest).max(0) as u64));
        FairnessSnapshot {
            policy,
            queued: self.queued,
            lineages: self.lineages,
            oldest_wait_ms,
            served: self.served,
            max_wait_ms: self.max_wait_ms,
            promoted: self.promoted,
            longest_side: self.longest_side,
            consecutive_side: self.consecutive_side,
            decisions: self.decisions,
        }
    }
}

impl FairnessSnapshot {
    /// Oldest queued wait of one tier, in ms.
    pub fn oldest(&self, tier: Tier) -> u64 {
        self.oldest_wait_ms[tier.index()]
    }

    /// Requests of one tier dispatched so far.
    pub fn served(&self, tier: Tier) -> u64 {
        self.served[tier.index()]
    }
}

/// The dispatch queue: tiers, line and agent rotation, the installed policy and
/// the fairness counters the broker publishes.
///
/// It is owned by the single dispatch loop, so it needs no lock. Every queued
/// request stays reachable: `is_empty` is false exactly while a push is
/// outstanding and no pop consumed it.
pub struct DispatchQueue<T> {
    policy: Arc<dyn BrokerPolicy>,
    cursor: PolicyCursor,
    tiers: [TierQueue<T>; TIERS],
    len: usize,
    served: [u64; TIERS],
    max_wait_ms: [u64; TIERS],
    promoted: u64,
}

impl<T> DispatchQueue<T> {
    pub fn new(policy: Arc<dyn BrokerPolicy>) -> Self {
        Self {
            policy,
            cursor: PolicyCursor::default(),
            tiers: std::array::from_fn(|_| TierQueue::new()),
            len: 0,
            served: [0; TIERS],
            max_wait_ms: [0; TIERS],
            promoted: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push(
        &mut self,
        facts: DispatchFacts,
        enqueued_at_ms: i64,
        payload: T,
    ) {
        let tier = facts.effective_tier();
        self.tiers[tier.index()].push(Queued {
            key: facts.key,
            scope: facts.scope,
            promoted: facts.is_promoted(),
            enqueued_at_ms,
            payload,
        });
        self.len += 1;
    }

    /// The pressure the policy decides over at `now_ms`.
    pub fn pressure(&self, now_ms: i64) -> QueuePressure {
        let mut pressure = QueuePressure::default();
        for tier in [Tier::Low, Tier::Normal, Tier::Subject, Tier::Urgent] {
            let queue = &self.tiers[tier.index()];
            pressure.ready[tier.index()] = queue.len;
            pressure.lineages[tier.index()] = queue.rotation.len();
            pressure.oldest_ms[tier.index()] = queue.oldest_wait(now_ms);
        }
        pressure
    }

    /// Dispatch the next request, or `None` when nothing is queued. A policy
    /// that declines (or names a tier with no ready request) falls back to the
    /// longest-waiting tier, so queued work can never stall behind a policy.
    pub fn pop(&mut self, now_ms: i64) -> Option<Dispatched<T>> {
        let pressure = self.pressure(now_ms);
        if !pressure.any_ready() {
            return None;
        }
        let mut chosen = self
            .policy
            .choose(&pressure, &mut self.cursor)
            .filter(|tier| pressure.ready(*tier) > 0)
            .or_else(|| pressure.longest_waiting())?;
        let mut queued = self.tiers[chosen.index()].pop_one();
        if queued.is_none() {
            // Integrity guard, not a policy decision: the chosen tier reported
            // ready but held no reachable request. Try the longest-waiting
            // ready tier once; never panic the resident dispatch loop and
            // never keep claiming work that cannot be handed out.
            if let Some(alternate) = pressure.longest_waiting().filter(|tier| *tier != chosen) {
                chosen = alternate;
                queued = self.tiers[chosen.index()].pop_one();
            }
        }
        let queued = queued?;
        self.len -= 1;
        self.cursor.record(chosen);
        if queued.promoted {
            self.promoted = self.promoted.saturating_add(1);
        }
        let waited_ms = (now_ms - queued.enqueued_at_ms).max(0) as u64;
        self.served[chosen.index()] = self.served[chosen.index()].saturating_add(1);
        self.max_wait_ms[chosen.index()] = self.max_wait_ms[chosen.index()].max(waited_ms);
        Some(Dispatched {
            payload: queued.payload,
            tier: chosen,
            waited_ms,
        })
    }

    /// Take one still-waiting request out of the queue by identity and return
    /// its payload, or `None` when it is not queued any more.
    ///
    /// This is the scheduler half of `queued → terminal(cancelled before
    /// dispatch)` (§41.9). The queue deliberately does not fail, drop or settle
    /// anything itself: whoever removes the request owns the durable decision
    /// and the never-dispatched proof.
    pub fn cancel(&mut self, key: EntityId) -> Option<T> {
        for tier in [Tier::Urgent, Tier::Subject, Tier::Normal, Tier::Low] {
            if let Some(payload) = self.tiers[tier.index()].remove(key) {
                self.len -= 1;
                return Some(payload);
            }
        }
        None
    }

    /// The counters: live queue state plus the service history. Waits are
    /// derived from the stored enqueue times by `FairnessCounters::snapshot`,
    /// so reading them later never reports a stale age.
    pub fn counters(&self) -> FairnessCounters {
        let mut oldest_enqueued_ms = [None; TIERS];
        let mut queued = [0_usize; TIERS];
        let mut lineages = [0_usize; TIERS];
        for tier in [Tier::Low, Tier::Normal, Tier::Subject, Tier::Urgent] {
            let queue = &self.tiers[tier.index()];
            queued[tier.index()] = queue.len;
            lineages[tier.index()] = queue.rotation.len();
            oldest_enqueued_ms[tier.index()] = queue.oldest_enqueued_ms;
        }
        FairnessCounters {
            queued,
            lineages,
            oldest_enqueued_ms,
            served: self.served,
            max_wait_ms: self.max_wait_ms,
            promoted: self.promoted,
            longest_side: self.cursor.longest_side,
            consecutive_side: self.cursor.consecutive_side,
            decisions: self.cursor.decisions,
        }
    }

    pub fn snapshot(&self, now_ms: i64) -> FairnessSnapshot {
        self.counters().snapshot(self.policy.name(), now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(seed: u8) -> EntityId {
        EntityId::derived("shenko.policy.test.agent", &[seed])
    }

    fn lineage(seed: u8) -> EntityId {
        EntityId::derived("shenko.policy.test.lineage", &[seed])
    }

    fn automatic() -> Arc<dyn BrokerPolicy> {
        Arc::new(AutomaticBrokerPolicy::default())
    }

    fn queue() -> DispatchQueue<u32> {
        DispatchQueue::new(automatic())
    }

    fn request(payload: u32) -> EntityId {
        EntityId::derived("shenko.policy.test.request", &[payload as u8])
    }

    fn facts(
        priority: InferencePriority,
        scope: FairnessScope,
        inherited: Option<InferencePriority>,
        payload: u32,
    ) -> DispatchFacts {
        DispatchFacts { key: request(payload), priority, scope, inherited }
    }

    fn push(
        queue: &mut DispatchQueue<u32>,
        priority: InferencePriority,
        scope: FairnessScope,
        at: i64,
        payload: u32,
    ) {
        queue.push(facts(priority, scope, None, payload), at, payload);
    }

    /// The tier index the queue uses is the priority's own numeric order, so
    /// no translation table can silently reorder the lanes.
    #[test]
    fn tier_order_matches_priority_order() {
        for tier in [Tier::Low, Tier::Normal, Tier::Subject, Tier::Urgent] {
            assert_eq!(tier.index(), tier.priority() as usize);
            assert_eq!(Tier::of(tier.priority()), tier);
        }
    }

    /// Identical offer and identical demand on both sides produce an
    /// alternating service order. No share is configured anywhere.
    #[test]
    fn symmetric_main_and_child_demand_alternates() {
        let mut queue = queue();
        for index in 0..5_u32 {
            push(&mut queue, InferencePriority::Subject, FairnessScope::per_agent(agent(1)), 0, index);
            push(&mut queue, InferencePriority::Normal, FairnessScope::per_agent(agent(2)), 0, 100 + index);
        }
        let mut order = Vec::new();
        while let Some(dispatched) = queue.pop(1_000) {
            order.push(dispatched.payload < 100);
        }
        assert_eq!(
            order,
            vec![true, false, true, false, true, false, true, false, true, false]
        );
        assert!(queue.is_empty());
    }

    /// One agent's backlog cannot take a second turn ahead of another agent
    /// that already had work queued in the same line.
    #[test]
    fn one_flooding_agent_cannot_hold_the_child_side() {
        let mut queue = queue();
        let line = lineage(1);
        let flooder = FairnessScope::new(line, agent(1));
        for (payload, scope) in [
            (1, flooder),
            (2, flooder),
            (3, FairnessScope::new(line, agent(2))),
            (4, FairnessScope::new(line, agent(3))),
            (5, flooder),
            (6, flooder),
        ] {
            push(&mut queue, InferencePriority::Normal, scope, 0, payload);
        }
        let mut order = Vec::new();
        while let Some(dispatched) = queue.pop(1_000) {
            order.push(dispatched.payload);
        }
        // Round-robin over agents: the flooder's second turn only comes after
        // each other ready agent was served once.
        assert_eq!(order, vec![1, 3, 4, 2, 5, 6]);
    }

    /// Each tier rotates agents independently, so an urgent flood from one
    /// agent also cannot delay another agent's urgent request.
    #[test]
    fn per_agent_rotation_applies_to_every_tier() {
        let mut queue = queue();
        let line = lineage(2);
        for payload in 0..3_u32 {
            push(&mut queue, InferencePriority::Urgent, FairnessScope::new(line, agent(1)), 0, payload);
        }
        push(&mut queue, InferencePriority::Urgent, FairnessScope::new(line, agent(2)), 0, 9);
        let first = queue.pop(1_000).expect("urgent work is queued");
        let second = queue.pop(1_000).expect("urgent work is queued");
        assert_eq!((first.payload, second.payload), (0, 9));
        assert_eq!(first.tier, Tier::Urgent);
    }

    /// Ordinary work that already waited past the deadline runs ahead of
    /// fresh urgent work; the same queue prefers urgent when the bound is
    /// disabled.
    #[test]
    fn starvation_deadline_preempts_fresh_urgent_work() {
        let build = |policy: Arc<dyn BrokerPolicy>| {
            let mut queue = DispatchQueue::new(policy);
            push(&mut queue, InferencePriority::Low, FairnessScope::per_agent(agent(1)), 0, 1);
            push(&mut queue, InferencePriority::Urgent, FairnessScope::per_agent(agent(2)), 39_990, 2);
            queue
        };
        let mut bounded = build(automatic());
        assert_eq!(bounded.pop(40_000).map(|dispatched| dispatched.payload), Some(1));

        let disabled = AutomaticBrokerPolicy {
            max_urgent_streak: 1,
            max_consecutive_side: 2,
            starvation_deadline_ms: 0,
        };
        let mut unbounded = build(Arc::new(disabled));
        assert_eq!(unbounded.pop(40_000).map(|dispatched| dispatched.payload), Some(2));
    }

    /// Urgent work opens a turn but never two in a row while ordinary work is
    /// ready, and nothing is lost in either direction.
    #[test]
    fn urgent_burst_is_bounded_and_ordinary_work_is_served() {
        let mut queue = queue();
        for payload in 0..4_u32 {
            push(&mut queue, InferencePriority::Urgent, FairnessScope::per_agent(agent(1)), 0, payload);
        }
        push(&mut queue, InferencePriority::Subject, FairnessScope::per_agent(agent(2)), 0, 100);
        let mut order = Vec::new();
        while let Some(dispatched) = queue.pop(1_000) {
            order.push(dispatched.payload);
        }
        assert_eq!(order, vec![0, 100, 1, 2, 3]);
    }

    /// SPEC §41.9: while both sides stay runnable, one side never runs more
    /// than two complete generations in a row even though its backlog is older
    /// than every child request — the latency comparison alone would keep
    /// serving it.
    #[test]
    fn consecutive_same_side_service_is_bounded() {
        let mut queue = queue();
        for payload in 0..5_u32 {
            push(&mut queue, InferencePriority::Subject, FairnessScope::per_agent(agent(1)), 0, payload);
        }
        for payload in 100..102_u32 {
            push(&mut queue, InferencePriority::Normal, FairnessScope::per_agent(agent(2)), 500, payload);
        }
        let mut order = Vec::new();
        while let Some(dispatched) = queue.pop(1_000) {
            order.push(dispatched.payload);
        }
        let sides: Vec<Side> = order.iter().map(|payload| if *payload < 100 { Side::Main } else { Side::Child }).collect();
        let mut run = 1;
        for window in sides.windows(2) {
            run = if window[0] == window[1] { run + 1 } else { 1 };
            assert!(run <= 2, "one side ran {run} generations in a row: {sides:?}");
        }
        assert_eq!(sides.first(), Some(&Side::Main), "the older backlog still goes first");
        assert_eq!(
            order,
            vec![0, 1, 100, 2, 3, 101, 4],
            "both child requests run while the older backlog is still queued"
        );
    }

    /// Recursive fan-out does not buy the parent line extra turns: a line
    /// holding three agents rotates as one line against another line, and its
    /// agents take turns only inside it.
    #[test]
    fn recursive_children_do_not_multiply_a_lineage_turn() {
        let mut queue = queue();
        let recursive = lineage(1);
        let other = lineage(2);
        push(&mut queue, InferencePriority::Normal, FairnessScope::new(recursive, agent(1)), 0, 1);
        push(&mut queue, InferencePriority::Normal, FairnessScope::new(recursive, agent(2)), 0, 2);
        push(&mut queue, InferencePriority::Normal, FairnessScope::new(recursive, agent(3)), 0, 3);
        push(&mut queue, InferencePriority::Normal, FairnessScope::new(other, agent(4)), 0, 4);
        push(&mut queue, InferencePriority::Normal, FairnessScope::new(other, agent(4)), 0, 5);
        let mut order = Vec::new();
        while let Some(dispatched) = queue.pop(1_000) {
            order.push(dispatched.payload);
        }
        assert_eq!(order, vec![1, 4, 2, 5, 3]);
    }

    /// The subject keeps its own turn while child work — including recursive
    /// descendants — is queued.
    #[test]
    fn recursive_children_rotate_with_the_subject() {
        let mut queue = queue();
        let root = lineage(10);
        let descendant = lineage(11);
        let sibling = lineage(12);
        push(&mut queue, InferencePriority::Subject, FairnessScope::per_agent(agent(1)), 0, 1);
        for step in 0..3_u32 {
            push(&mut queue, InferencePriority::Normal, FairnessScope::new(root, agent(2)), 0, 10 + step);
            push(&mut queue, InferencePriority::Low, FairnessScope::new(descendant, agent(3)), 0, 20 + step);
            push(&mut queue, InferencePriority::Normal, FairnessScope::new(sibling, agent(4)), 0, 30 + step);
        }
        let mut seen = Vec::new();
        while let Some(dispatched) = queue.pop(1_000) {
            seen.push(dispatched.payload);
        }
        assert_eq!(seen.len(), 10);
        assert_eq!(seen[0], 1, "the subject turn is not delayed by child backlog");
        let mut first_cycle = seen[1..4].to_vec();
        first_cycle.sort_unstable();
        assert_eq!(first_cycle, vec![10, 20, 30]);
    }

    /// A policy that declines never stalls queued work: the queue falls back
    /// to its longest-waiting tier, and the fallback still moves the fairness
    /// cursor.
    #[test]
    fn declining_policy_never_stalls_ready_work() {
        struct Abstain;
        impl BrokerPolicy for Abstain {
            fn name(&self) -> &'static str {
                "abstain"
            }
            fn choose(&self, _pressure: &QueuePressure, _cursor: &mut PolicyCursor) -> Option<Tier> {
                None
            }
        }
        let mut queue = DispatchQueue::new(Arc::new(Abstain));
        push(&mut queue, InferencePriority::Subject, FairnessScope::per_agent(agent(1)), 900, 1);
        push(&mut queue, InferencePriority::Low, FairnessScope::per_agent(agent(2)), 0, 2);
        assert_eq!(queue.pop(1_000).map(|dispatched| dispatched.payload), Some(2));
        assert_eq!(queue.pop(1_000).map(|dispatched| dispatched.payload), Some(1));
        assert!(queue.is_empty());
        assert_eq!(queue.counters().decisions, 2, "fallbacks are recorded");
    }

    /// The published counters carry the queue wait, the subject's maximum wait
    /// and the number of lands per side — the SPEC §13.6 minimum.
    #[test]
    fn snapshot_reports_queue_wait_and_service_counts() {
        let mut queue = queue();
        push(&mut queue, InferencePriority::Subject, FairnessScope::per_agent(agent(1)), 0, 1);
        push(&mut queue, InferencePriority::Normal, FairnessScope::per_agent(agent(2)), 0, 2);
        let queued = queue.snapshot(250);
        assert_eq!(queued.policy, "automatic.pressure-balanced");
        assert_eq!(queued.queued, [0, 1, 1, 0]);
        assert_eq!(queued.oldest(Tier::Subject), 250);
        assert_eq!(queued.served(Tier::Subject), 0);

        let dispatched = queue.pop(250).expect("subject work is queued");
        assert_eq!(dispatched.waited_ms, 250, "the dispatch reports the real wait");
        assert_eq!(dispatched.tier, Tier::Subject);
        let served = queue.snapshot(1_000);
        assert_eq!(served.served(Tier::Subject), 1);
        assert_eq!(served.max_wait_ms[Tier::Subject.index()], 250);
        assert_eq!(served.longest_side, Some(Side::Main));
        assert_eq!(served.consecutive_side, 1);
        assert_eq!(served.lineages[Tier::Normal.index()], 1);
    }

    /// Dependency inheritance (§13.3 step 1): a request that unblocks a parked
    /// subject-class task files under the subject tier, so rotation cannot
    /// leave the parked task waiting behind an unrelated child line. Without
    /// the declared inherited class the same two requests keep rotation order.
    #[test]
    fn inherited_class_outranks_plain_rotation() {
        let mut queue = queue();
        let parked_child = InferencePriority::Normal;
        push(&mut queue, parked_child, FairnessScope::per_agent(agent(1)), 0, 1);
        queue.push(
            facts(parked_child, FairnessScope::per_agent(agent(2)), Some(InferencePriority::Subject), 2),
            0,
            2,
        );
        let mut order = Vec::new();
        while let Some(dispatched) = queue.pop(1_000) {
            order.push(dispatched.payload);
        }
        assert_eq!(order, vec![2, 1], "the parked parent's child runs first");

        let mut plain = queue();
        push(&mut plain, parked_child, FairnessScope::per_agent(agent(1)), 0, 1);
        push(&mut plain, parked_child, FairnessScope::per_agent(agent(2)), 0, 2);
        let mut order = Vec::new();
        while let Some(dispatched) = plain.pop(1_000) {
            order.push(dispatched.payload);
        }
        assert_eq!(order, vec![1, 2], "without inheritance the two lines rotate");
    }

    /// An inherited class never borrows the external interruption tier, and the
    /// promotion is counted for evidence instead of being silent.
    #[test]
    fn inherited_class_is_clamped_below_urgent() {
        let mut queue = queue();
        push(&mut queue, InferencePriority::Normal, FairnessScope::per_agent(agent(1)), 0, 1);
        queue.push(
            facts(InferencePriority::Normal, FairnessScope::per_agent(agent(2)), Some(InferencePriority::Urgent), 0),
            0,
            2,
        );
        let promoted = queue.pop(1_000).expect("a request is queued");
        assert_eq!(promoted.tier, Tier::Subject, "inheritance stops at the subject tier");
        assert_eq!(queue.counters().promoted, 1);
        assert_eq!(queue.pop(1_000).map(|dispatched| dispatched.payload), Some(1));
    }

    /// Queued cancellation removes exactly the named request, keeps the rest of
    /// the rotation intact, and reports a request that already left as absent.
    #[test]
    fn queued_cancel_removes_only_the_named_request() {
        let mut queue = queue();
        let line = lineage(3);
        let flooder = FairnessScope::new(line, agent(1));
        for payload in [1_u32, 2, 3] {
            push(&mut queue, InferencePriority::Normal, flooder, 0, payload);
        }
        push(&mut queue, InferencePriority::Low, FairnessScope::per_agent(agent(2)), 0, 4);
        assert_eq!(queue.cancel(request(2)), Some(2), "the waiting request is removed");
        assert_eq!(queue.cancel(request(2)), None, "the same identity is not removed twice");
        assert_eq!(queue.counters().queued, [1, 2, 0, 0]);
        let mut order = Vec::new();
        while let Some(dispatched) = queue.pop(1_000) {
            order.push(dispatched.payload);
        }
        assert_eq!(order, vec![1, 4, 3], "the surviving rotation order is unchanged");

        let mut alone = queue();
        push(&mut alone, InferencePriority::Low, FairnessScope::per_agent(agent(9)), 0, 9);
        assert!(alone.pop(1_000).is_some());
        assert_eq!(
            alone.cancel(request(9)),
            None,
            "a request that already left the queue is not cancellable here"
        );
    }
}
