//! QuotaPolicy and its replayable decisions (docs/specs/quota-resources.md#spec-42-5). A decision picks the
//! model profile and the highest admission rate every quota window it
//! touches can sustain until that window's period ends. It is an upper bound
//! on pacing, never a demand: no request is created to use up a balance.
use std::collections::{HashMap, HashSet};

/// Which readings of one observation source may feed a forecast, and how.
/// A source absent from this list is never admitted as evidence; missing
/// TTL or an unstated stale policy is never guessed as "always fresh".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceObservationPolicy {
    pub source_kind: crate::model::SourceKind,
    /// The exact source contract this admission covers, when the policy
    /// admits one contract of that kind. `None` admits every contract of the
    /// kind. Two contracts of one kind are different facts: a configured
    /// limit document and a usage report are never the same evidence.
    #[serde(default)]
    pub contract: Option<String>,
    /// How long a reading from this source stays current.
    pub ttl: Duration,
    /// What a reading that is no longer fresh may still plan.
    pub stale: crate::forecast::StaleUse,
}

impl SourceObservationPolicy {
    pub fn options(&self) -> crate::forecast::ForecastOptions {
        crate::forecast::ForecastOptions { ttl: self.ttl, stale: self.stale }
    }

    /// Whether this admission covers a reading of the given contract.
    pub fn admits_contract(&self, contract: &str) -> bool {
        match &self.contract {
            None => true,
            Some(pinned) => pinned == contract,
        }
    }
}

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
use shenko_types::commit::CommitRef;
use shenko_types::object::Digest;

use crate::forecast::{AccountedAttempt, Planning, ProviderExhaustionEvidence, QuotaForecast, SourceValidity, WindowAssumptions};
use crate::model::Range;

/// One quota window of one account at one provider.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WindowRef {
    pub provider_id: String,
    pub account_scope_ref: String,
    pub window_key: String,
}

/// A request's cost range in the named quota window's actual unit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowCost {
    pub window: WindowRef,
    pub unit: String,
    pub amount: Range,
}

/// Planning inputs for one account-scoped window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScopedAssumptions {
    pub window: WindowRef,
    pub value: WindowAssumptions,
}

/// Actual backend availability, not an inferred quota or permission state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendState {
    Available,
    Unavailable,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileAvailability {
    pub profile_id: String,
    pub state: BackendState,
}

/// A configured model profile. `smart`/`cheap`/`local` are names, not a
/// guarantee of quality or price; costs are per request, in each window's
/// own unit, learned or conservatively estimated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelProfile {
    pub profile_id: String,
    pub model_ref: String,
    /// Runs on a local backend and draws on no provider quota.
    pub local: bool,
    pub cost_per_request: Vec<WindowCost>,
    /// Effort levels this profile may actually run at, in order of
    /// preference for the current task's needs, each priced in its own
    /// windows (docs/specs/quota-resources.md#spec-42-3 `model_profiles[].effort_bounds`). A policy
    /// recorded before effort planning has none: it then runs at the single
    /// implicit level priced by `cost_per_request`, which is exactly what
    /// every earlier decision did. The levels are configured facts about
    /// what the backend supports, never a guess at a quality or price order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effort_levels: Vec<EffortLevel>,
}

/// One supported effort level of a profile and what one request at that
/// level costs, per window's own unit (docs/specs/quota-resources.md#spec-42-3).
///
/// The identifier is the configured level name (for example `low`,
/// `standard`, `deep`); this crate never ranks levels by the name, so a
/// policy states its preference by the order of `effort_levels`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffortLevel {
    pub effort: String,
    pub cost_per_request: Vec<WindowCost>,
}

impl ModelProfile {
    /// The request costs of this profile at one effort level.
    ///
    /// `None` names the implicit level every pre-effort policy runs at, which
    /// is `cost_per_request` itself. A named level must be declared by this
    /// profile: an unknown name is `None` rather than another level's price,
    /// so a decision can never be charged at a level it did not select.
    pub fn costs_at(&self, effort: Option<&str>) -> Option<&[WindowCost]> {
        match effort {
            None => Some(self.cost_per_request.as_slice()),
            Some(effort) => self
                .effort_levels
                .iter()
                .find(|level| level.effort == effort)
                .map(|level| level.cost_per_request.as_slice()),
        }
    }

    /// The effort levels the planner may choose from, in preference order.
    /// A profile without declared levels has exactly one, unnamed, level.
    fn plan_levels(&self) -> Vec<Option<&EffortLevel>> {
        if self.effort_levels.is_empty() {
            vec![None]
        } else {
            self.effort_levels.iter().map(Some).collect()
        }
    }
}

/// What to do when a window a profile needs has no usable forecast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownQuota {
    /// Treat the profile as unusable for now (observation_blocked).
    Block,
    /// Admit without a quota bound; the decision records that it did.
    Admit,
}

/// What the planner optimizes within the authorized budget (docs/specs/quota-resources.md#spec-42-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyObjective {
    /// Spread the available budget across the period at its sustainable
    /// rate, honoring the target utilization range when one is configured.
    /// This is the behavior every policy had before the field existed.
    #[default]
    Balanced,
    /// Aim at the upper end of the target utilization range: useful work
    /// early, still bounded by the range. Requires that range.
    TargetUtilization,
    /// Aim at the lower end of the target utilization range so the window is
    /// not exhausted before its reset; without a range it paces at the
    /// sustainable rate.
    PreserveUntilReset,
}

impl PolicyObjective {
    /// Whether this is the pre-existing, byte-identical default.
    pub fn is_balanced(&self) -> bool {
        *self == Self::Balanced
    }
}

/// An absolute per-window hold, in that window's own unit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowReserve {
    pub window: WindowRef,
    /// The unit the amount is stated in. A reserve stated in another unit
    /// than the profile's cost cannot be priced, and the window stays unknown
    /// instead of mixing units (docs/specs/quota-resources.md#spec-42-3 rule 2).
    pub unit: String,
    pub amount: Range,
}

/// Allowance to admit above the sustainable pacing rate for a short time.
/// It never grants budget: the window's held budget still bounds admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionBurst {
    pub max_requests: u32,
    pub within: Duration,
}

/// When the withheld emergency reserve may be released: inside the last
/// `release_within` before the window's period end (or bucket refill).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UrgentReserveRelease {
    pub release_within: Duration,
}

/// Admission pacing inputs (docs/specs/quota-resources.md#spec-42-3). Every field has a planner or
/// admission effect; none is recorded for decoration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionPacing {
    /// Whether the planner emits per-window pacing rates at all. `false`
    /// admits as fast as each window's held budget allows.
    pub enabled: bool,
    /// Short burst allowance above the sustainable rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<AdmissionBurst>,
    /// Minimum spacing between two admissions that charge the same window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_interval: Option<Duration>,
    /// When the held emergency reserve may be spent instead of idling to the
    /// end of the period.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urgent_reserve: Option<UrgentReserveRelease>,
}

impl Default for AdmissionPacing {
    fn default() -> Self {
        // The pre-existing behavior: pacing on, no extra refinements.
        Self { enabled: true, burst: None, minimum_interval: None, urgent_reserve: None }
    }
}

impl AdmissionPacing {
    /// Whether this is the pre-existing default, which serializes to no key.
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaPolicy {
    pub policy_id: EntityId,
    pub revision: u64,
    /// Profiles in order of preference for the current task's needs; only
    /// profiles meeting the task's quality floor belong here.
    pub profiles: Vec<ModelProfile>,
    pub assumptions: Vec<ScopedAssumptions>,
    /// Admissions per hour below which a cloud profile is not worth choosing
    /// over an available local one.
    pub min_progress_per_hour: f64,
    /// Fractional advantage required before switching to a different
    /// profile than the previous decision (anti-flapping).
    pub switching_hysteresis: f64,
    pub on_unknown: UnknownQuota,
    /// Extra paid usage beyond the plan. Never enabled by the optimizer; only
    /// a spending authority the user granted can set it.
    pub allow_overage: bool,
    pub spending_authority_ref: Option<String>,
    /// How long a decision stays valid before it must be recomputed.
    pub decision_ttl: Duration,
    /// Which observation sources this policy admits, with their lifetime and
    /// stale-use rules. An empty list (including older serialized policies)
    /// admits no observation as forecast evidence; everything stays unknown.
    #[serde(default)]
    pub observation_sources: Vec<SourceObservationPolicy>,
    /// What the planner optimizes within the authorized budget (docs/specs/quota-resources.md#spec-42-3).
    /// The default `balanced` is the behavior every earlier policy already
    /// had, so a legacy policy re-serializes to its recorded bytes.
    #[serde(default, skip_serializing_if = "PolicyObjective::is_balanced")]
    pub objective: PolicyObjective,
    /// Intended share of each window's planning budget to use by the period
    /// end, as a fraction of that budget. A planning target, never a promise
    /// that exactly this share is spent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_utilization_range: Option<Range>,
    /// How far ahead a decision may plan. Caps the decision's validity on top
    /// of the source lifetime and the window boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planning_horizon: Option<Duration>,
    /// Per-window amount withheld from planning and admission. It stays
    /// unspent unless the window is inside its urgent release and the policy
    /// names one. Absolute, in that window's own unit — distinct from the
    /// per-window `assumptions` reserve/margin, which are planning inputs of
    /// the forecast and not a guaranteed hold.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emergency_reserve_by_window: Vec<WindowReserve>,
    /// Extra fraction of each window's planning budget withheld for
    /// uncertainty (0..=1). Relative, unlike an absolute window reserve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncertainty_margin: Option<f64>,
    /// Admission pacing inputs (docs/specs/quota-resources.md#spec-42-3).
    #[serde(default, skip_serializing_if = "AdmissionPacing::is_default")]
    pub admission_pacing: AdmissionPacing,
}

impl QuotaPolicy {
    /// The forecast options bound to one observation source, or None when
    /// this policy does not admit that source as evidence at all.
    pub fn forecast_options(&self, source_kind: crate::model::SourceKind)
        -> Option<crate::forecast::ForecastOptions>
    {
        self.observation_sources
            .iter()
            .find(|entry| entry.source_kind == source_kind)
            .map(SourceObservationPolicy::options)
    }

    /// The forecast options bound to one source contract, or None when this
    /// policy does not admit that contract as evidence at all.
    pub fn forecast_options_for(
        &self,
        source_kind: crate::model::SourceKind,
        contract: &str,
    ) -> Option<crate::forecast::ForecastOptions> {
        self.observation_sources
            .iter()
            .find(|entry| entry.source_kind == source_kind && entry.admits_contract(contract))
            .map(SourceObservationPolicy::options)
    }

    /// The admission entry that covers one source contract.
    pub fn source_admission(
        &self,
        source_kind: crate::model::SourceKind,
        contract: &str,
    ) -> Option<&SourceObservationPolicy> {
        self.observation_sources
            .iter()
            .find(|entry| entry.source_kind == source_kind && entry.admits_contract(contract))
    }

    /// The model a decided profile actually runs, for the request seal that
    /// follows the decision. The decision chooses a profile; the seal binds
    /// the profile's real model.
    pub fn model_for_profile(&self, profile_id: &str) -> Option<&str> {
        self.profiles
            .iter()
            .find(|profile| profile.profile_id == profile_id)
            .map(|profile| profile.model_ref.as_str())
    }

    /// Whether the planner emits per-window pacing rates.
    pub fn pacing_enabled(&self) -> bool {
        self.admission_pacing.enabled
    }

    /// The share of each window's sustainable rate this objective plans to
    /// use. The sustainable rate already spends the held budget evenly over
    /// the period, so the factor only moves inside the configured target
    /// utilization range; it is never a claim about the provider's capacity.
    pub fn pacing_factor(&self) -> f64 {
        let factor = match (self.objective, self.target_utilization_range) {
            // Deliberately conservative: use only the guaranteed part of the
            // target so the window is not exhausted before its reset.
            (PolicyObjective::PreserveUntilReset, Some(range)) => range.lower,
            (_, Some(range)) => range.upper,
            // No utilization target: spend the held budget evenly (the
            // pre-existing behavior). `TargetUtilization` without a range is
            // rejected by validation and also lands here.
            _ => 1.0,
        };
        if factor.is_finite() { factor.clamp(0.0, 1.0) } else { 1.0 }
    }

    /// The amount withheld from one window's planning budget and admission
    /// ceiling, in that window's unit.
    ///
    /// The emergency reserve is held for the whole period and released only
    /// inside the policy's urgent window, so end-of-period idleness is
    /// avoided without spending the reserve early. `uncertainty_margin` is a
    /// relative extra hold against the planning budget. Both are distinct
    /// from `assumptions` reserve/margin, which are absolute per-window
    /// inputs the forecast already subtracted: this is a policy-level hold
    /// that survives whatever assumptions a reading carries.
    ///
    /// Returns `None` when the hold cannot be priced (mixing units, an
    /// unrepresentable amount, or a non-finite budget): the window then stays
    /// unknown instead of being planned from a guessed number.
    pub fn window_hold(
        &self,
        window: &WindowRef,
        unit: &str,
        available: f64,
        boundary: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Option<f64> {
        if !available.is_finite() || available < 0.0 {
            return None;
        }
        let mut hold = 0.0_f64;
        for reserve in self
            .emergency_reserve_by_window
            .iter()
            .filter(|reserve| &reserve.window == window)
        {
            if reserve.unit != unit {
                return None;
            }
            let amount = Range::new(reserve.amount.lower, reserve.amount.upper)?;
            let released = self
                .admission_pacing
                .urgent_reserve
                .zip(boundary)
                .is_some_and(|(urgent, boundary)| {
                    now.checked_add_signed(urgent.release_within)
                        .is_some_and(|deadline| boundary <= deadline)
                });
            if !released {
                hold += amount.upper; if !hold.is_finite() { return None; }
            }
        }
        if let Some(margin) = self.uncertainty_margin {
            if !margin.is_finite() || !(0.0..=1.0).contains(&margin) {
                return None;
            }
            hold += available * margin; if !hold.is_finite() { return None; }
        }
        hold.is_finite().then_some(hold.min(available))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Choice {
    /// Use this cloud profile at most `rate_per_hour` admissions per hour.
    /// None means no periodic rate bound; immediate window budgets still apply.
    Cloud {
        profile_id: String,
        /// The effort level this request runs at (docs/specs/quota-resources.md#spec-42-3, docs/specs/quota-resources.md#spec-42-6). A
        /// decision recorded before effort planning names none and is charged
        /// at the profile's own `cost_per_request`, exactly as before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        rate_per_hour: Option<f64>,
        unbounded_windows: Vec<WindowRef>,
    },
    Local { profile_id: String },
    /// Budget exists only after a reset; wait for it (quota_policy_wait).
    Wait { until: Option<DateTime<Utc>> },
    /// The provider itself reports these windows at their limit.
    ProviderExhausted { windows: Vec<WindowRef>, until: Option<DateTime<Utc>> },
    /// Needed windows have no usable reading and policy blocks on unknown.
    ObservationBlocked { windows: Vec<WindowRef> },
    /// No configured profile can run at all: a capacity fact, not quota.
    NoBackend,
    /// At least one relevant backend's availability has not been observed.
    CapacityUnknown { profiles: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowConstraint {
    pub window: WindowRef,
    pub unit: String,
    /// None only when the configured policy explicitly admits unknown quota.
    pub available: Option<f64>,
    /// Period pacing for this window; buckets and unknowns have no rate.
    pub rate_per_hour: Option<f64>,
    pub next_change: Option<DateTime<Utc>>,
    pub accounted_attempts: Vec<AccountedAttempt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyError {
    InvalidTiming,
    InvalidRate,
    InvalidProfile,
    DuplicateProfile,
    MissingCloudCost,
    InvalidCost,
    InvalidAssumption,
    /// A utilization target is missing, out of range, or inverted.
    InvalidTarget,
    /// A withheld reserve or uncertainty margin cannot be priced.
    InvalidReserve,
    /// A pacing burst, interval or urgent-release input is unusable.
    InvalidPacing,
    UnsupportedOverage,
    InvalidSerialization,
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "quota policy error: {self:?}")
    }
}

impl std::error::Error for PolicyError {}

/// Persisted record of one decision and why it was made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyDecision {
    pub decision_id: EntityId,
    pub policy_id: EntityId,
    pub policy_revision: u64,
    /// Exact policy bytes, not merely a reusable revision number.
    pub policy_digest: Digest,
    pub based_on_forecasts: Vec<EntityId>,
    /// Actual profile availability observed when the decision was made.
    pub availability: Vec<ProfileAvailability>,
    pub decided_at: DateTime<Utc>,
    pub valid_until: DateTime<Utc>,
    pub choice: Choice,
    /// Per-window budget and accounting evidence for the selected profile.
    pub constraints: Vec<WindowConstraint>,
    /// Sustainable rate per considered profile (None: unbounded/unknown).
    pub considered: Vec<(String, Option<f64>)>,
    /// The routing strategy that chose this decision, as `id@version`. A
    /// replacement algorithm is therefore attributable during replay instead
    /// of silently reinterpreting an older period. Decisions recorded before
    /// the replaceable-strategy seam keep it absent: unknown, never guessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    /// How many requests of each profile the *held* planning budget supports
    /// over the horizon (docs/specs/quota-resources.md#spec-42-3 `subject_round_estimates`). A conservative
    /// bound derived from recorded evidence, never a promise that exactly
    /// this many rounds happen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_estimates: Option<RoundEstimates>,
    /// One row per quota-bearing profile the planner assessed: the planned
    /// rate, the round estimate and the cost ranges it was priced with
    /// (docs/specs/quota-resources.md#spec-42-3 `scenarios`). What the planner compared, kept with the
    /// decision that used it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scenarios: Vec<PlanScenario>,
}

/// Rounds the held budget supports per profile (docs/specs/quota-resources.md#spec-42-3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoundEstimates {
    /// The horizon these numbers cover: the policy's planning horizon, else
    /// the decision's own validity window.
    pub horizon: Duration,
    /// The smallest priced cloud profile's estimate, or `None` when no cloud
    /// profile could be priced from recorded evidence.
    pub cloud_only: Option<f64>,
    /// The smallest estimate across every priced profile (cloud or local).
    pub all_models: Option<f64>,
    /// One entry per configured profile; `None` when any of its windows is
    /// unknown, waiting or unbounded.
    pub per_profile: Vec<(String, Option<f64>)>,
}

/// One planning scenario the decision compared (docs/specs/quota-resources.md#spec-42-3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanScenario {
    pub profile_id: String,
    /// The effort level this row was priced at (docs/specs/quota-resources.md#spec-42-3 `effort_policy`).
    /// `None` is the implicit level of a policy recorded before effort
    /// planning, and is omitted so such a decision keeps its bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The planned admission rate for this profile, `None` when unbounded or
    /// not paceable.
    pub rate_per_hour: Option<f64>,
    pub rounds: Option<f64>,
    /// The per-window cost ranges this scenario was priced with.
    pub cost_per_request: Vec<WindowCost>,
    /// Useful work this scenario's rounds are expected to produce
    /// (docs/specs/quota-resources.md#spec-42-3 `expected_useful_work`), when durable evidence supports an
    /// expectation at all. Absent stays unknown: one request is never
    /// assumed to equal one useful outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_useful_work: Option<UsefulWorkEstimate>,
}

/// What one counted useful outcome is. Units are never merged: an estimate
/// stated in committed subject steps is not a count of child results, and
/// neither is a quota window's unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkUnit {
    /// A subject step whose S1 action was durably committed: the subject
    /// actually advanced instead of only spending a round.
    CommittedSubjectStep,
}

impl WorkUnit {
    /// The stable name written into records and read by the subject.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CommittedSubjectStep => "committed_subject_step",
        }
    }
}

/// Measured useful work per admitted subject round, read from durable
/// provenance (docs/specs/quota-resources.md#spec-42-3 `expected_useful_work`, docs/specs/quota-resources.md#spec-42-5).
///
/// The caller counts what the ledger actually recorded — terminal request
/// rows as rounds and committed steps as useful outcomes — and points at the
/// ledger position it counted them at. Nothing here is a model's opinion, a
/// `knowledge_kind`, a keyword score or a constant: no evidence means no
/// expectation, and the reading is a mechanism measurement rather than a
/// cognition attitude (docs/specs/cognition-units.md#spec-51-1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkEvidence {
    pub unit: WorkUnit,
    /// Rounds that reached a recorded terminal outcome.
    pub rounds_observed: u64,
    /// Of those rounds' steps, how many durably committed their action.
    pub useful_observed: u64,
    /// The ledger position the counts were read at: a real, re-readable
    /// pointer to the evidence rather than a description of it.
    pub evidence: CommitRef,
}

impl WorkEvidence {
    /// Whether this evidence can state a rate at all: it needs observed
    /// rounds and counts that fit the unit.
    pub fn is_usable(&self) -> bool {
        self.rounds_observed > 0 && self.useful_observed <= self.rounds_observed
    }

    /// Useful outcomes per further round, as a range.
    ///
    /// With `u` useful outcomes over `n` rounds the next round is placed in
    /// the successor interval `[u/(n+1), (u+1)/(n+1)]`: deliberately wide
    /// while the evidence is thin, narrowing as rounds accumulate, and never
    /// `1.0` merely because a round happened. `None` for unusable evidence.
    pub fn per_round(&self) -> Option<Range> {
        if !self.is_usable() {
            return None;
        }
        let denominator = self.rounds_observed as f64 + 1.0;
        Range::new(
            self.useful_observed as f64 / denominator,
            (self.useful_observed as f64 + 1.0) / denominator,
        )
    }
}

/// Expected useful work over a scenario's horizon, in an explicit unit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsefulWorkEstimate {
    pub unit: WorkUnit,
    /// Expected useful outcomes the held budget buys, as a range.
    pub expected: Range,
    /// The measured counts and their provenance, so the expectation can be
    /// re-derived instead of believed.
    pub rounds_observed: u64,
    pub useful_observed: u64,
    pub evidence: CommitRef,
}

impl UsefulWorkEstimate {
    /// The expectation of `rounds` further admitted subject rounds under
    /// measured evidence, or `None` when either the round estimate or the
    /// measured rate is unknown.
    fn of(rounds: Option<f64>, work: Option<&WorkEvidence>) -> Option<Self> {
        let rounds = rounds?;
        let work = work?;
        let per_round = work.per_round()?;
        if !rounds.is_finite() || rounds < 0.0 {
            return None;
        }
        // Outward rounding: the expectation may not look more certain than
        // the measured counts it came from.
        let scale = |value: f64, upward: bool| {
            let product = rounds * value;
            if !product.is_finite() {
                None
            } else if upward {
                Some(product.next_up())
            } else {
                Some(product.next_down())
            }
        };
        let expected = Range::new(scale(per_round.lower, false)?, scale(per_round.upper, true)?)?;
        Some(Self {
            unit: work.unit,
            expected,
            rounds_observed: work.rounds_observed,
            useful_observed: work.useful_observed,
            evidence: work.evidence,
        })
    }
}

/// Requests one profile's held window budgets support, or `None` when any of
/// its windows has no usable budget or its cost cannot be represented.
fn rounds_estimate(profile: &ModelProfile, constraints: &[WindowConstraint]) -> Option<f64> {
    rounds_estimate_with(profile, profile.costs_at(None)?, constraints)
}

/// Requests one profile's held window budgets support at one effort level's
/// priced costs.
fn rounds_estimate_with(
    profile: &ModelProfile,
    costs: &[WindowCost],
    constraints: &[WindowConstraint],
) -> Option<f64> {
    if profile.local {
        return None;
    }
    if costs.is_empty() {
        return None;
    }
    let mut rounds = f64::MAX;
    for cost in costs {
        let available = constraints.iter().find(|constraint| constraint.window == cost.window)
            .and_then(|constraint| constraint.available)?;
        if !available.is_finite() || !(cost.amount.upper > 0.0) {
            return None;
        }
        rounds = rounds.min(available / cost.amount.upper);
    }
    rounds.is_finite().then(|| rounds.max(0.0))
}

/// The smallest priced estimate; `None` when nothing could be priced.
fn smallest(entries: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    entries.flatten().fold(None::<f64>, |acc, value| {
        Some(acc.map_or(value, |prior| prior.min(value)))
    })
}

/// How one window constrains one profile.
enum Bound {
    Rate { per_hour: f64, constraint: WindowConstraint },
    Bucket { constraint: WindowConstraint },
    Short { until: Option<DateTime<Utc>>, provider_at_limit: bool },
    Unknown,
}

fn bound(policy: &QuotaPolicy, forecasts: &[QuotaForecast], window: &WindowRef, cost: &WindowCost,
    expected: WindowAssumptions, now: DateTime<Utc>) -> Bound {
    let mut matches = forecasts
        .iter()
        .filter(|f| f.provider_id == window.provider_id && f.account_scope_ref == window.account_scope_ref)
        .flat_map(|f| f.windows.iter().map(move |w| (f, w)))
        .filter(|(_, w)| w.window_key == window.window_key);
    let Some((forecast, w)) = matches.next() else { return Bound::Unknown };
    // Without an ordering/coverage proof two competing forecasts cannot be
    // resolved by whichever happens to appear first.
    let mut attempts = HashSet::new();
    if matches.next().is_some() || forecast.computed_at > now || w.unit != cost.unit || w.assumptions != expected
        || forecast.freshness == crate::model::Freshness::Unknown || w.basis == crate::model::Freshness::Unknown
        || w.basis != forecast.freshness
        || !(matches!((forecast.source_validity, forecast.freshness),
            (SourceValidity::FreshUntil { exclusive }, crate::model::Freshness::Fresh) if now < exclusive)
            || matches!((forecast.source_validity, forecast.freshness),
                (SourceValidity::LastKnown, crate::model::Freshness::Stale)))
        || w.accounted_attempts.iter().any(|a| !a.cost_upper.is_finite() || a.cost_upper < 0.0
            || !attempts.insert((a.request_id, a.ledger_commit)))
        || [w.remaining, w.unreflected_own_cost, w.external_cost].into_iter().flatten()
            .any(|range| Range::new(range.lower, range.upper).is_none())
    {
        return Bound::Unknown;
    }
    let constraint = |available, rate_per_hour, next_change| WindowConstraint {
        window: window.clone(), unit: cost.unit.clone(), available: Some(available), next_change,
        rate_per_hour,
        accounted_attempts: w.accounted_attempts.clone(),
    };
    // A last-known reading may still be used for planning, but it cannot
    // establish that the provider is currently exhausted.
    let provider_at_limit = forecast.freshness == crate::model::Freshness::Fresh
        && w.provider_exhaustion == ProviderExhaustionEvidence::ReportedEmpty
        && w.remaining.is_some_and(|remaining| remaining.upper == 0.0);
    match &w.planning {
        Planning::Period { available, per_hour, resets_at }
            if available.is_finite() && *available >= 0.0 && per_hour.is_finite() && *per_hour >= 0.0
                && *resets_at > now =>
        {
            let Some(remaining) = w.remaining else { return Bound::Unknown };
            let duration = *resets_at - forecast.computed_at;
            let seconds = duration.num_seconds() as f64 + duration.subsec_nanos() as f64 / 1_000_000_000.0;
            if seconds <= 0.0 || *available > remaining.upper || *per_hour > *available / (seconds / 3600.0) {
                return Bound::Unknown;
            }
            // The policy's own hold (emergency reserve, uncertainty margin)
            // is withheld from both the admission ceiling and the planned
            // rate: it survives whatever assumptions the reading carries.
            let Some(hold) = policy.window_hold(window, &cost.unit, *available, Some(*resets_at), now) else {
                return Bound::Unknown;
            };
            let held = (*available - hold).max(0.0);
            if held < cost.amount.upper || *per_hour == 0.0 {
                // A short caused only by the policy's hold is a policy wait,
                // never evidence that the provider reached its limit.
                return Bound::Short { until: Some(*resets_at), provider_at_limit: provider_at_limit && hold == 0.0 };
            }
            let factor = policy.pacing_factor();
            let ratio = if *available > 0.0 { held / *available } else { 0.0 };
            let rate = (per_hour / cost.amount.upper) * factor * ratio;
            if !rate.is_finite() {
                return Bound::Unknown;
            }
            if rate <= 0.0 {
                // The plan deliberately spends nothing yet (a zero target or
                // a full hold): that is a planned wait at the boundary, not
                // an unknown reading.
                return Bound::Short { until: Some(*resets_at), provider_at_limit: false };
            }
            let rate = rate.next_down();
            Bound::Rate { per_hour: rate, constraint: constraint(held, Some(rate), Some(*resets_at)) }
        }
        Planning::Replenishing { remaining, refill_by } => {
            let Some(remaining) = Range::new(remaining.lower, remaining.upper) else { return Bound::Unknown };
            let Some(reported) = w.remaining else { return Bound::Unknown };
            if remaining.upper > reported.upper { return Bound::Unknown; }
            let Some(hold) = policy.window_hold(window, &cost.unit, remaining.lower, *refill_by, now) else {
                return Bound::Unknown;
            };
            let held = (remaining.lower - hold).max(0.0);
            if held < cost.amount.upper {
                // The observed zero no longer proves the current provider
                // limit after its known refill boundary. Do not assume a
                // replenished balance either; await a new observation.
                let refill_passed = refill_by.is_some_and(|at| at <= now);
                Bound::Short { until: if refill_passed { None } else { *refill_by },
                    provider_at_limit: provider_at_limit && !refill_passed && hold == 0.0 }
            } else {
                Bound::Bucket { constraint: constraint(held, None, *refill_by) }
            }
        }
        Planning::Unknown { .. } => Bound::Unknown,
        _ => Bound::Unknown,
    }
}

/// Outcome of checking one cloud profile against every window it uses.
struct Assessment {
    rate: Option<f64>,
    unknown: Vec<WindowRef>,
    exhausted: Vec<WindowRef>,
    wait_until: Option<DateTime<Utc>>,
    waiting: bool,
    constraints: Vec<WindowConstraint>,
}

fn assess(costs: &[WindowCost], forecasts: &[QuotaForecast], policy: &QuotaPolicy, now: DateTime<Utc>) -> Assessment {
    let mut a = Assessment { rate: None, unknown: Vec::new(), exhausted: Vec::new(), wait_until: None,
        waiting: false, constraints: Vec::new() };
    let later = |t: Option<DateTime<Utc>>, a: &mut Assessment| {
        a.wait_until = match (a.wait_until, t) {
            (Some(x), Some(y)) => Some(x.max(y)),
            (x, y) => x.or(y),
        };
    };
    // Pacing can be switched off for a policy that prefers to spend each
    // window's held budget as fast as it allows; the budget ceiling stays.
    let pacing = policy.pacing_enabled();
    for cost in costs {
        let window = &cost.window;
        let expected = policy.assumptions.iter().find(|entry| &entry.window == window)
            .map_or(WindowAssumptions::default(), |entry| entry.value);
        match bound(policy, forecasts, window, cost, expected, now) {
            Bound::Rate { per_hour, mut constraint } => {
                if pacing {
                    a.rate = Some(a.rate.map_or(per_hour, |r| r.min(per_hour)));
                } else {
                    // No periodic rate: admission is bounded by the held
                    // window budget and its boundary only.
                    constraint.rate_per_hour = None;
                }
                a.constraints.push(constraint);
            }
            Bound::Bucket { constraint } => a.constraints.push(constraint),
            Bound::Short { until, provider_at_limit } => {
                a.waiting = true;
                later(until, &mut a);
                if provider_at_limit { a.exhausted.push(window.clone()); }
            }
            Bound::Unknown => {
                a.unknown.push(window.clone());
                a.constraints.push(WindowConstraint { window: window.clone(), unit: cost.unit.clone(),
                    available: None, rate_per_hour: None, next_change: None, accounted_attempts: Vec::new() });
            }
        }
    }
    a
}

pub(crate) fn validate(policy: &QuotaPolicy, now: DateTime<Utc>) -> Result<DateTime<Utc>, PolicyError> {
    if policy.decision_ttl <= Duration::zero() { return Err(PolicyError::InvalidTiming); }
    let mut valid_until = now.checked_add_signed(policy.decision_ttl).ok_or(PolicyError::InvalidTiming)?;
    // A planning horizon bounds how far ahead one decision may plan. It can
    // only shorten the validity the decision would have had anyway.
    if let Some(horizon) = policy.planning_horizon {
        if horizon <= Duration::zero() { return Err(PolicyError::InvalidTiming); }
        valid_until = valid_until.min(now.checked_add_signed(horizon).ok_or(PolicyError::InvalidTiming)?);
    }
    // The utilization target is a planned share of each window's budget: a
    // fraction of the plan, never a claim about the provider's total. An
    // objective that aims at a target must name one instead of silently
    // planning at the sustainable rate.
    match policy.target_utilization_range {
        Some(range) => {
            if Range::new(range.lower, range.upper).is_none()
                || !range.lower.is_finite() || !range.upper.is_finite()
                || range.lower < 0.0 || range.upper > 1.0
            {
                return Err(PolicyError::InvalidTarget);
            }
        }
        None if policy.objective == PolicyObjective::TargetUtilization => {
            return Err(PolicyError::InvalidTarget)
        }
        None => {}
    }
    if let Some(margin) = policy.uncertainty_margin {
        if !margin.is_finite() || !(0.0..=1.0).contains(&margin) {
            return Err(PolicyError::InvalidReserve);
        }
    }
    let mut reserved = HashSet::new();
    for reserve in &policy.emergency_reserve_by_window {
        if reserve.unit.trim().is_empty()
            || Range::new(reserve.amount.lower, reserve.amount.upper).is_none()
            || !reserve.amount.lower.is_finite() || !reserve.amount.upper.is_finite()
            || reserve.amount.lower < 0.0
            || !reserved.insert(&reserve.window)
        {
            return Err(PolicyError::InvalidReserve);
        }
    }
    if let Some(burst) = policy.admission_pacing.burst {
        if burst.max_requests == 0 || burst.within <= Duration::zero() {
            return Err(PolicyError::InvalidPacing);
        }
    }
    if policy.admission_pacing.minimum_interval.is_some_and(|interval| interval <= Duration::zero()) {
        return Err(PolicyError::InvalidPacing);
    }
    if policy
        .admission_pacing
        .urgent_reserve
        .is_some_and(|urgent| urgent.release_within <= Duration::zero())
    {
        return Err(PolicyError::InvalidPacing);
    }
    if !policy.min_progress_per_hour.is_finite() || policy.min_progress_per_hour < 0.0
        || !policy.switching_hysteresis.is_finite() || policy.switching_hysteresis < 0.0
        || !(policy.min_progress_per_hour * (1.0 + policy.switching_hysteresis)).is_finite()
    {
        return Err(PolicyError::InvalidRate);
    }
    // Overage requires a separately enforced spending-authority and limit;
    // this pure policy slice must not silently pretend to provide either.
    if policy.allow_overage { return Err(PolicyError::UnsupportedOverage); }
    let mut assumption_windows = HashSet::new();
    for entry in &policy.assumptions {
        if !entry.value.valid() || !assumption_windows.insert(&entry.window) {
            return Err(PolicyError::InvalidAssumption);
        }
    }
    let mut names = HashSet::new();
    for (index, source) in policy.observation_sources.iter().enumerate() {
        if source.ttl <= Duration::zero()
            || policy.observation_sources.iter().take(index).any(|earlier| {
                earlier.source_kind == source.source_kind && earlier.contract == source.contract
            })
        {
            return Err(PolicyError::InvalidAssumption);
        }
    }
    let mut windows = HashSet::new();
    for profile in &policy.profiles {
        if profile.profile_id.trim().is_empty() || profile.model_ref.trim().is_empty()
            || (profile.local && (!profile.cost_per_request.is_empty() || !profile.effort_levels.is_empty()))
        {
            return Err(PolicyError::InvalidProfile);
        }
        if !names.insert(&profile.profile_id) { return Err(PolicyError::DuplicateProfile); }
        if !profile.local && profile.cost_per_request.is_empty() { return Err(PolicyError::MissingCloudCost); }
        // Every declared effort level is priced like the profile itself: a
        // level with an unusable price could never be planned or admitted, and
        // an ambiguous name would let a decision name a level that does not
        // exist.
        let mut levels = HashSet::new();
        let mut priced: Vec<&[WindowCost]> = vec![profile.cost_per_request.as_slice()];
        for level in &profile.effort_levels {
            if level.effort.trim().is_empty() || level.effort.trim() != level.effort
                || !levels.insert(&level.effort)
                || level.cost_per_request.is_empty()
            {
                return Err(PolicyError::InvalidCost);
            }
            priced.push(level.cost_per_request.as_slice());
        }
        for costs in priced {
            windows.clear();
            for cost in costs {
                let window = &cost.window;
                if window.provider_id.trim().is_empty() || window.account_scope_ref.trim().is_empty()
                    || window.window_key.trim().is_empty() || cost.unit.trim().is_empty()
                    || Range::new(cost.amount.lower, cost.amount.upper).is_none() || cost.amount.upper <= 0.0
                    || !windows.insert(window)
                {
                    return Err(PolicyError::InvalidCost);
                }
            }
        }
    }
    Ok(valid_until)
}

/// Exact content identity of a policy, binding a sealed decision to the
/// policy bytes it was computed from rather than a reusable revision number.
pub fn policy_digest(policy: &QuotaPolicy) -> Result<Digest, PolicyError> {
    let mut hash = blake3::Hasher::new();
    serde_json::to_writer(&mut hash, policy).map_err(|_| PolicyError::InvalidSerialization)?;
    Ok(Digest(format!("blake3:{}", hash.finalize().to_hex())))
}

struct Candidate {
    profile_id: String,
    /// The effort level this candidate is priced at; `None` for the implicit
    /// level of a policy recorded before effort planning.
    effort: Option<String>,
    rate: Option<f64>,
    unbounded: Vec<WindowRef>,
    constraints: Vec<WindowConstraint>,
}

impl Candidate {
    fn score(&self) -> f64 { self.rate.unwrap_or(f64::MAX) }
}

/// One profile's supported effort levels and what the evidence says about
/// each. A level is either priceable now, deliberately waiting, or blocked by
/// evidence the policy refuses to guess.
enum LevelState {
    Candidate(Candidate),
    Waiting(Assessment),
    Blocked(Assessment),
}

fn level_state(
    policy: &QuotaPolicy,
    profile_id: &str,
    effort: Option<String>,
    assessment: Assessment,
) -> LevelState {
    if !assessment.unknown.is_empty() && policy.on_unknown == UnknownQuota::Block {
        return LevelState::Blocked(assessment);
    }
    if assessment.waiting || assessment.rate == Some(0.0) {
        return LevelState::Waiting(assessment);
    }
    let unbounded = assessment.unknown;
    LevelState::Candidate(Candidate {
        profile_id: profile_id.to_owned(),
        effort,
        rate: assessment.rate,
        unbounded,
        constraints: assessment.constraints,
    })
}

/// Choose from actual backend observations and bounded quota evidence. A
/// missing backend observation stays unknown; no profile is assumed present.
///
/// This is the entry point for a runtime with no measured useful-work
/// evidence: every scenario then records its expectation as unknown.
pub fn decide(
    policy: &QuotaPolicy,
    forecasts: &[QuotaForecast],
    availability: &HashMap<String, BackendState>,
    previous: Option<&PolicyDecision>,
    now: DateTime<Utc>,
) -> Result<PolicyDecision, PolicyError> {
    decide_with(policy, forecasts, availability, previous, None, now)
}

/// [`decide`] with the measured useful work of an admitted subject round.
///
/// `work` is durable provenance the caller read, not an estimate this crate
/// may invent: without it a scenario states no expectation, and it is never
/// replaced by "one request is one useful outcome".
pub fn decide_with(
    policy: &QuotaPolicy,
    forecasts: &[QuotaForecast],
    availability: &HashMap<String, BackendState>,
    previous: Option<&PolicyDecision>,
    work: Option<&WorkEvidence>,
    now: DateTime<Utc>,
) -> Result<PolicyDecision, PolicyError> {
    let mut valid_until = validate(policy, now)?;
    // Every available profile's evidence can affect the choice. Recompute
    // before a consulted source expires or a window changes, including when
    // a different window sets a later aggregate wait deadline.
    for profile in &policy.profiles {
        if availability.get(&profile.profile_id) != Some(&BackendState::Available) { continue; }
        let mut costs: Vec<&WindowCost> = profile.cost_per_request.iter().collect();
        for level in &profile.effort_levels {
            costs.extend(level.cost_per_request.iter());
        }
        for cost in costs {
            for forecast in forecasts.iter().filter(|f| f.provider_id == cost.window.provider_id
                && f.account_scope_ref == cost.window.account_scope_ref) {
                for window in forecast.windows.iter().filter(|w| w.window_key == cost.window.window_key) {
                    if let SourceValidity::FreshUntil { exclusive } = forecast.source_validity {
                        if forecast.freshness == crate::model::Freshness::Fresh && exclusive > now {
                            valid_until = valid_until.min(exclusive);
                        }
                    }
                    let boundary = match &window.planning {
                        Planning::Period { resets_at, .. } => Some(*resets_at),
                        Planning::Replenishing { refill_by, .. } => *refill_by,
                        Planning::Unknown { .. } => None,
                    };
                    if let Some(at) = boundary.filter(|at| *at > now) {
                        valid_until = valid_until.min(at);
                    }
                }
            }
        }
    }
    let digest = policy_digest(policy)?;
    let previous_choice = previous.filter(|d| d.policy_id == policy.policy_id && d.policy_revision == policy.revision
        && d.policy_digest == digest
        && d.decided_at <= now && now < d.valid_until).and_then(|d| match &d.choice {
        Choice::Cloud { profile_id, effort, .. } => {
            Some((profile_id.as_str(), effort.as_deref()))
        }
        Choice::Local { profile_id } => Some((profile_id.as_str(), None)),
        _ => None,
    });
    let previous_profile = previous_choice.map(|(profile, _)| profile);
    let mut considered = Vec::new();
    let mut feasible = Vec::<Candidate>::new();
    let mut slow: Option<Candidate> = None;
    let mut exhausted = Vec::new();
    // A zero-rate profile not at a provider limit is only waiting on policy
    // (reserve, margin, refill); that outranks reporting exhaustion.
    let mut policy_wait = false;
    let mut blocked = Vec::new();
    let mut wait_until: Option<DateTime<Utc>> = None;
    let mut local = None;
    let mut unknown_backends = Vec::new();
    // What the plan can price, per assessed profile. A profile that was never
    // assessed (unknown or unavailable backend) stays absent, so its estimate
    // is unknown instead of zero.
    let mut estimates: Vec<(String, Option<f64>)> = Vec::new();
    let mut scenarios: Vec<PlanScenario> = Vec::new();

    for profile in &policy.profiles {
        match availability.get(&profile.profile_id).copied().unwrap_or(BackendState::Unknown) {
            BackendState::Unavailable => continue,
            BackendState::Unknown => { unknown_backends.push(profile.profile_id.clone()); continue; }
            BackendState::Available => {}
        }
        if profile.local {
            if local.is_none() || previous_profile == Some(profile.profile_id.as_str()) {
                local = Some(profile.profile_id.clone());
            }
            continue;
        }
        // One assessment per supported effort level, in the profile's own
        // preference order. A level's own windows decide its own budget: a
        // deeper level that charges another window is factored in, not
        // assumed free.
        let mut levels: Vec<(Option<String>, Assessment)> = Vec::new();
        for level in profile.plan_levels() {
            let effort = level.map(|level| level.effort.as_str());
            let Some(costs) = profile.costs_at(effort) else { continue };
            levels.push((level.map(|level| level.effort.clone()), assess(costs, forecasts, policy, now)));
        }
        // What the planner compared is recorded per profile/effort pair, with
        // the useful work its rounds are expected to produce when durable
        // evidence supports an expectation at all.
        let mut rounds_by_level: Vec<(Option<String>, Option<f64>)> = Vec::new();
        for (effort, assessment) in &levels {
            let costs = profile.costs_at(effort.as_deref()).unwrap_or(&[]);
            let rounds = rounds_estimate_with(profile, costs, &assessment.constraints);
            rounds_by_level.push((effort.clone(), rounds));
            scenarios.push(PlanScenario {
                profile_id: profile.profile_id.clone(),
                effort: effort.clone(),
                rate_per_hour: assessment.rate,
                rounds,
                cost_per_request: costs.to_vec(),
                expected_useful_work: UsefulWorkEstimate::of(rounds, work),
            });
        }
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut waiting: Vec<Assessment> = Vec::new();
        let mut blocked_levels: Vec<Assessment> = Vec::new();
        for (effort, assessment) in levels {
            match level_state(policy, &profile.profile_id, effort, assessment) {
                LevelState::Candidate(candidate) => candidates.push(candidate),
                LevelState::Waiting(assessment) => waiting.push(assessment),
                LevelState::Blocked(assessment) => blocked_levels.push(assessment),
            }
        }
        if candidates.is_empty() {
            // No level of this profile can be planned now: report the
            // profile's own evidence, exactly as a single-level policy did.
            let rounds = rounds_by_level.first().and_then(|(_, rounds)| *rounds);
            estimates.push((profile.profile_id.clone(), rounds));
            if !blocked_levels.is_empty() {
                considered.push((profile.profile_id.clone(), None));
                for assessment in blocked_levels {
                    blocked.extend(assessment.unknown);
                }
                continue;
            }
            considered.push((
                profile.profile_id.clone(),
                waiting.first().and_then(|assessment| assessment.rate),
            ));
            for assessment in waiting {
                policy_wait |= assessment.exhausted.is_empty();
                for window in assessment.exhausted {
                    if !exhausted.contains(&window) { exhausted.push(window); }
                }
                wait_until = match (wait_until, assessment.wait_until) {
                    (Some(x), Some(y)) => Some(x.min(y)),
                    (x, y) => x.or(y),
                };
            }
            continue;
        }
        // Prefer the most preferred feasible level; keep the level the
        // previous decision used unless a more preferred one clears the
        // switching margin, so the same plan is not re-chosen every round.
        let threshold = policy.min_progress_per_hour;
        let preferred = candidates
            .iter()
            .position(|candidate| candidate.score() >= threshold)
            .or_else(|| {
                candidates
                    .iter()
                    .enumerate()
                    .fold(None::<(usize, f64)>, |best, (index, candidate)| {
                        let score = candidate.score();
                        Some(match best {
                            Some((best_index, best_score)) if best_score >= score => {
                                (best_index, best_score)
                            }
                            _ => (index, score),
                        })
                    })
                    .map(|(index, _)| index)
            });
        let chosen = previous_choice
            .filter(|(name, _)| *name == profile.profile_id.as_str())
            .and_then(|(_, effort)| {
                let previous_level = candidates
                    .iter()
                    .position(|candidate| candidate.effort.as_deref() == effort)?;
                Some(match preferred {
                    Some(preferred)
                        if preferred != previous_level
                            && candidates[preferred].score()
                                >= threshold * (1.0 + policy.switching_hysteresis) =>
                    {
                        preferred
                    }
                    _ => previous_level,
                })
            })
            .or(preferred)
            .and_then(|index| candidates.into_iter().nth(index));
        let Some(candidate) = chosen else { continue };
        let rounds = rounds_by_level
            .iter()
            .find(|(effort, _)| effort == &candidate.effort)
            .and_then(|(_, rounds)| *rounds);
        estimates.push((profile.profile_id.clone(), rounds));
        considered.push((profile.profile_id.clone(), candidate.rate));
        if candidate.score() >= policy.min_progress_per_hour {
            feasible.push(candidate);
        } else if slow.as_ref().is_none_or(|other| candidate.score() > other.score()) {
            slow = Some(candidate);
        }
    }

    // Prefer the first feasible profile, but keep the previous one unless the
    // preferred one clears the minimum by the hysteresis margin.
    let keep = previous_profile.and_then(|p| feasible.iter().find(|candidate| candidate.profile_id == p));
    let first = feasible.first();
    let pick = match (keep, first) {
        (Some(k), Some(f)) if f.profile_id != k.profile_id && f.score() >= policy.min_progress_per_hour * (1.0 + policy.switching_hysteresis) => {
            Some(f)
        }
        (Some(k), _) => Some(k),
        (None, f) => f,
    };
    blocked.sort();
    blocked.dedup();
    exhausted.sort();
    exhausted.dedup();
    // Local profiles are not in `feasible`. Retain the previous available local
    // choice across marginal cloud recovery, using the same switching margin.
    // An unavailable local never reaches `local`, so it cannot pin the choice.
    let keep_local = previous_profile.is_some() && local.as_deref() == previous_profile
        && first.is_some_and(|candidate| candidate.score()
            < policy.min_progress_per_hour * (1.0 + policy.switching_hysteresis));
    let selected = if keep_local { None }
        else { pick.or_else(|| if local.is_none() { slow.as_ref() } else { None }) };
    let (choice, constraints) = if let Some(candidate) = selected {
        (Choice::Cloud { profile_id: candidate.profile_id.clone(), effort: candidate.effort.clone(),
            rate_per_hour: candidate.rate, unbounded_windows: candidate.unbounded.clone() },
            candidate.constraints.clone())
    } else if let Some(id) = local {
        (Choice::Local { profile_id: id }, Vec::new())
    } else if !unknown_backends.is_empty() {
        (Choice::CapacityUnknown { profiles: unknown_backends }, Vec::new())
    } else if policy_wait {
        (Choice::Wait { until: wait_until }, Vec::new())
    } else if !exhausted.is_empty() {
        (Choice::ProviderExhausted { windows: exhausted, until: wait_until }, Vec::new())
    } else if !blocked.is_empty() {
        (Choice::ObservationBlocked { windows: blocked }, Vec::new())
    } else {
        (Choice::NoBackend, Vec::new())
    };
    // Rounds the held budget supports over the horizon. `None` where the
    // evidence cannot price a profile: no zero, no guess. The horizon is the
    // policy's planning horizon, else the decision's own validity window.
    let horizon = policy.planning_horizon.unwrap_or(policy.decision_ttl);
    let per_profile: Vec<(String, Option<f64>)> = policy
        .profiles
        .iter()
        .map(|profile| {
            (
                profile.profile_id.clone(),
                estimates
                    .iter()
                    .find(|(id, _)| id == &profile.profile_id)
                    .and_then(|(_, rounds)| *rounds),
            )
        })
        .collect();
    let cloud_only = smallest(
        policy
            .profiles
            .iter()
            .filter(|profile| !profile.local)
            .map(|profile| per_profile.iter().find(|(id, _)| id == &profile.profile_id).and_then(|(_, rounds)| *rounds)),
    );
    let all_models = smallest(per_profile.iter().map(|(_, rounds)| *rounds));
    let round_estimates = all_models.map(|_| RoundEstimates {
        horizon,
        cloud_only,
        all_models,
        per_profile,
    });
    Ok(PolicyDecision {
        decision_id: EntityId::new(),
        policy_id: policy.policy_id,
        policy_revision: policy.revision,
        policy_digest: digest,
        based_on_forecasts: forecasts.iter().map(|f| f.forecast_id).collect(),
        availability: policy.profiles.iter().map(|profile| ProfileAvailability {
            profile_id: profile.profile_id.clone(),
            state: availability.get(&profile.profile_id).copied().unwrap_or(BackendState::Unknown),
        }).collect(),
        decided_at: now,
        valid_until,
        choice,
        constraints,
        considered,
        strategy: None,
        round_estimates,
        scenarios,
    })
}
