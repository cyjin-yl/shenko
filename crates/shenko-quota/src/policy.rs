//! QuotaPolicy and its replayable decisions (§42.5). A decision picks the
//! model profile and the highest admission rate every quota window it
//! touches can sustain until that window's period ends. It is an upper bound
//! on pacing, never a demand: no request is created to use up a balance.
use std::collections::{HashMap, HashSet};

/// Which readings of one observation source may feed a forecast, and how.
/// A source absent from this list is never admitted as evidence; missing
/// TTL or an unstated stale policy is never guessed as "always fresh".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceObservationPolicy {
    pub source_kind: crate::model::SourceKind,
    /// How long a reading from this source stays current.
    pub ttl: Duration,
    /// What a reading that is no longer fresh may still plan.
    pub stale: crate::forecast::StaleUse,
}

impl SourceObservationPolicy {
    pub fn options(&self) -> crate::forecast::ForecastOptions {
        crate::forecast::ForecastOptions { ttl: self.ttl, stale: self.stale }
    }
}

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Choice {
    /// Use this cloud profile at most `rate_per_hour` admissions per hour.
    /// None means no periodic rate bound; immediate window budgets still apply.
    Cloud { profile_id: String, rate_per_hour: Option<f64>, unbounded_windows: Vec<WindowRef> },
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
}

/// How one window constrains one profile.
enum Bound {
    Rate { per_hour: f64, constraint: WindowConstraint },
    Bucket { constraint: WindowConstraint },
    Short { until: Option<DateTime<Utc>>, provider_at_limit: bool },
    Unknown,
}

fn bound(forecasts: &[QuotaForecast], window: &WindowRef, cost: &WindowCost,
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
            if *available < cost.amount.upper || *per_hour == 0.0 {
                return Bound::Short { until: Some(*resets_at), provider_at_limit };
            }
            let rate = per_hour / cost.amount.upper;
            if !rate.is_finite() || rate <= 0.0 { return Bound::Unknown; }
            let rate = rate.next_down();
            Bound::Rate { per_hour: rate, constraint: constraint(*available, Some(rate), Some(*resets_at)) }
        }
        Planning::Replenishing { remaining, refill_by } => {
            let Some(remaining) = Range::new(remaining.lower, remaining.upper) else { return Bound::Unknown };
            let Some(reported) = w.remaining else { return Bound::Unknown };
            if remaining.upper > reported.upper { return Bound::Unknown; }
            if remaining.lower < cost.amount.upper {
                // The observed zero no longer proves the current provider
                // limit after its known refill boundary. Do not assume a
                // replenished balance either; await a new observation.
                let refill_passed = refill_by.is_some_and(|at| at <= now);
                Bound::Short { until: if refill_passed { None } else { *refill_by },
                    provider_at_limit: provider_at_limit && !refill_passed }
            } else {
                Bound::Bucket { constraint: constraint(remaining.lower, None, *refill_by) }
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

fn assess(profile: &ModelProfile, forecasts: &[QuotaForecast], policy: &QuotaPolicy, now: DateTime<Utc>) -> Assessment {
    let mut a = Assessment { rate: None, unknown: Vec::new(), exhausted: Vec::new(), wait_until: None,
        waiting: false, constraints: Vec::new() };
    let later = |t: Option<DateTime<Utc>>, a: &mut Assessment| {
        a.wait_until = match (a.wait_until, t) {
            (Some(x), Some(y)) => Some(x.max(y)),
            (x, y) => x.or(y),
        };
    };
    for cost in &profile.cost_per_request {
        let window = &cost.window;
        let expected = policy.assumptions.iter().find(|entry| &entry.window == window)
            .map_or(WindowAssumptions::default(), |entry| entry.value);
        match bound(forecasts, window, cost, expected, now) {
            Bound::Rate { per_hour, constraint } => {
                a.rate = Some(a.rate.map_or(per_hour, |r| r.min(per_hour)));
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
    let valid_until = now.checked_add_signed(policy.decision_ttl).ok_or(PolicyError::InvalidTiming)?;
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
            || policy.observation_sources.iter().take(index).any(|earlier| earlier.source_kind == source.source_kind)
        {
            return Err(PolicyError::InvalidAssumption);
        }
    }
    let mut windows = HashSet::new();
    for profile in &policy.profiles {
        if profile.profile_id.trim().is_empty() || profile.model_ref.trim().is_empty()
            || (profile.local && !profile.cost_per_request.is_empty())
        {
            return Err(PolicyError::InvalidProfile);
        }
        if !names.insert(&profile.profile_id) { return Err(PolicyError::DuplicateProfile); }
        if !profile.local && profile.cost_per_request.is_empty() { return Err(PolicyError::MissingCloudCost); }
        windows.clear();
        for cost in &profile.cost_per_request {
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
    rate: Option<f64>,
    unbounded: Vec<WindowRef>,
    constraints: Vec<WindowConstraint>,
}

impl Candidate {
    fn score(&self) -> f64 { self.rate.unwrap_or(f64::MAX) }
}

/// Choose from actual backend observations and bounded quota evidence. A
/// missing backend observation stays unknown; no profile is assumed present.
pub fn decide(
    policy: &QuotaPolicy,
    forecasts: &[QuotaForecast],
    availability: &HashMap<String, BackendState>,
    previous: Option<&PolicyDecision>,
    now: DateTime<Utc>,
) -> Result<PolicyDecision, PolicyError> {
    let mut valid_until = validate(policy, now)?;
    // Every available profile's evidence can affect the choice. Recompute
    // before a consulted source expires or a window changes, including when
    // a different window sets a later aggregate wait deadline.
    for profile in &policy.profiles {
        if availability.get(&profile.profile_id) != Some(&BackendState::Available) { continue; }
        for cost in &profile.cost_per_request {
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
    let previous_profile = previous.filter(|d| d.policy_id == policy.policy_id && d.policy_revision == policy.revision
        && d.policy_digest == digest
        && d.decided_at <= now && now < d.valid_until).and_then(|d| match &d.choice {
        Choice::Cloud { profile_id, .. } | Choice::Local { profile_id } => Some(profile_id.as_str()),
        _ => None,
    });
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
        let a = assess(profile, forecasts, policy, now);
        if !a.unknown.is_empty() && policy.on_unknown == UnknownQuota::Block {
            considered.push((profile.profile_id.clone(), None));
            blocked.extend(a.unknown);
            continue;
        }
        if a.waiting || a.rate == Some(0.0) {
            policy_wait |= a.exhausted.is_empty();
            for window in a.exhausted {
                if !exhausted.contains(&window) { exhausted.push(window); }
            }
            wait_until = match (wait_until, a.wait_until) {
                (Some(x), Some(y)) => Some(x.min(y)),
                (x, y) => x.or(y),
            };
            considered.push((profile.profile_id.clone(), a.rate));
            continue;
        }
        let unbounded = if a.unknown.is_empty() {
            Vec::new()
        } else {
            a.unknown
        };
        let candidate = Candidate { profile_id: profile.profile_id.clone(), rate: a.rate,
            unbounded, constraints: a.constraints };
        considered.push((profile.profile_id.clone(), candidate.rate));
        if candidate.score() >= policy.min_progress_per_hour {
            feasible.push(candidate);
        } else {
            if slow.as_ref().is_none_or(|other| candidate.score() > other.score()) { slow = Some(candidate); }
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
        (Choice::Cloud { profile_id: candidate.profile_id.clone(), rate_per_hour: candidate.rate,
            unbounded_windows: candidate.unbounded.clone() }, candidate.constraints.clone())
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
    })
}
