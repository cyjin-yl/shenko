//! QuotaPolicy and its replayable decisions (§42.5). A decision picks the
//! model profile and the highest admission rate every quota window it
//! touches can sustain until that window's period ends. It is an upper bound
//! on pacing, never a demand: no request is created to use up a balance.
use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;

use crate::forecast::{Planning, QuotaForecast, WindowAssumptions};
use crate::model::Range;

/// One quota window of one account at one provider.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WindowRef {
    pub provider_id: String,
    pub account_scope_ref: String,
    pub window_key: String,
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
    pub cost_per_request: HashMap<WindowRef, Range>,
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
    pub assumptions: HashMap<WindowRef, WindowAssumptions>,
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Choice {
    /// Use this cloud profile at most `rate_per_hour` admissions per hour.
    Cloud { profile_id: String, rate_per_hour: f64, unbounded_windows: Vec<WindowRef> },
    Local { profile_id: String },
    /// Budget exists only after a reset; wait for it (quota_policy_wait).
    Wait { until: Option<DateTime<Utc>> },
    /// The provider itself reports these windows at their limit.
    ProviderExhausted { windows: Vec<WindowRef>, until: Option<DateTime<Utc>> },
    /// Needed windows have no usable reading and policy blocks on unknown.
    ObservationBlocked { windows: Vec<WindowRef> },
    /// No configured profile can run at all: a capacity fact, not quota.
    NoBackend,
}

/// Persisted record of one decision and why it was made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyDecision {
    pub decision_id: EntityId,
    pub policy_id: EntityId,
    pub policy_revision: u64,
    pub based_on_forecasts: Vec<EntityId>,
    pub decided_at: DateTime<Utc>,
    pub valid_until: DateTime<Utc>,
    pub choice: Choice,
    /// Sustainable rate per considered profile (None: unbounded/unknown).
    pub considered: Vec<(String, Option<f64>)>,
}

/// How one window constrains one profile.
enum Bound {
    /// Sustainable admissions per hour (0 means nothing until reset).
    Rate { per_hour: f64, resets_at: DateTime<Utc>, provider_at_limit: bool },
    /// A bucket that holds at least one request now.
    Bucket,
    /// A bucket short of one request until it refills.
    BucketShort { refill_by: Option<DateTime<Utc>> },
    Unknown,
}

fn bound(forecasts: &[QuotaForecast], window: &WindowRef, cost: Range) -> Bound {
    let found = forecasts
        .iter()
        .filter(|f| f.provider_id == window.provider_id && f.account_scope_ref == window.account_scope_ref)
        .flat_map(|f| f.windows.iter())
        .find(|w| w.window_key == window.window_key);
    let Some(w) = found else { return Bound::Unknown };
    if !(cost.upper.is_finite() && cost.lower >= 0.0 && cost.upper > 0.0) {
        return Bound::Unknown;
    }
    match &w.planning {
        Planning::Period { available, per_hour, resets_at } => Bound::Rate {
            per_hour: per_hour / cost.upper,
            resets_at: *resets_at,
            provider_at_limit: *available == 0.0 && w.remaining.is_some_and(|r| r.upper == 0.0),
        },
        Planning::Replenishing { remaining, .. } if remaining.lower >= cost.upper => Bound::Bucket,
        Planning::Replenishing { refill_by, .. } => Bound::BucketShort { refill_by: *refill_by },
        Planning::Unknown { .. } => Bound::Unknown,
    }
}

/// Outcome of checking one cloud profile against every window it uses.
struct Assessment {
    rate: Option<f64>,
    unknown: Vec<WindowRef>,
    exhausted: Vec<WindowRef>,
    wait_until: Option<DateTime<Utc>>,
}

fn assess(profile: &ModelProfile, forecasts: &[QuotaForecast]) -> Assessment {
    let mut a = Assessment { rate: None, unknown: Vec::new(), exhausted: Vec::new(), wait_until: None };
    let later = |t: Option<DateTime<Utc>>, a: &mut Assessment| {
        a.wait_until = match (a.wait_until, t) {
            (Some(x), Some(y)) => Some(x.max(y)),
            (x, y) => x.or(y),
        };
    };
    for (window, cost) in &profile.cost_per_request {
        match bound(forecasts, window, *cost) {
            Bound::Rate { per_hour, resets_at, provider_at_limit } => {
                a.rate = Some(a.rate.map_or(per_hour, |r| r.min(per_hour)));
                if per_hour == 0.0 {
                    later(Some(resets_at), &mut a);
                    if provider_at_limit {
                        a.exhausted.push(window.clone());
                    }
                }
            }
            Bound::Bucket => {}
            Bound::BucketShort { refill_by } => {
                a.rate = Some(0.0);
                later(refill_by, &mut a);
            }
            Bound::Unknown => a.unknown.push(window.clone()),
        }
    }
    a
}

/// Choose the profile and pacing for the next admissions. Pure: the same
/// policy, forecasts, backend availability and previous decision give the
/// same choice, so decisions can be replayed and audited.
pub fn decide(
    policy: &QuotaPolicy,
    forecasts: &[QuotaForecast],
    local_available: bool,
    previous: Option<&PolicyDecision>,
    now: DateTime<Utc>,
) -> PolicyDecision {
    let previous_profile = previous.and_then(|d| match &d.choice {
        Choice::Cloud { profile_id, .. } | Choice::Local { profile_id } => Some(profile_id.as_str()),
        _ => None,
    });
    let mut considered = Vec::new();
    let mut feasible: Vec<(String, f64, Vec<WindowRef>)> = Vec::new();
    let mut slow: Option<(String, f64, Vec<WindowRef>)> = None;
    let mut exhausted = Vec::new();
    // A zero-rate profile not at a provider limit is only waiting on policy
    // (reserve, margin, refill); that outranks reporting exhaustion.
    let mut policy_wait = false;
    let mut blocked = Vec::new();
    let mut wait_until: Option<DateTime<Utc>> = None;
    let mut local = None;

    for profile in &policy.profiles {
        if profile.local {
            if local_available && local.is_none() {
                local = Some(profile.profile_id.clone());
            }
            continue;
        }
        let a = assess(profile, forecasts);
        let unbounded = if a.unknown.is_empty() {
            Vec::new()
        } else if policy.on_unknown == UnknownQuota::Block {
            considered.push((profile.profile_id.clone(), None));
            blocked.extend(a.unknown);
            continue;
        } else {
            a.unknown
        };
        let rate = a.rate.unwrap_or(f64::INFINITY);
        considered.push((profile.profile_id.clone(), rate.is_finite().then_some(rate)));
        if rate >= policy.min_progress_per_hour {
            feasible.push((profile.profile_id.clone(), rate, unbounded));
        } else if rate > 0.0 {
            if slow.as_ref().is_none_or(|(_, r, _)| rate > *r) {
                slow = Some((profile.profile_id.clone(), rate, unbounded));
            }
        } else {
            policy_wait |= a.exhausted.is_empty();
            for w in a.exhausted {
                if !exhausted.contains(&w) {
                    exhausted.push(w);
                }
            }
            wait_until = match (wait_until, a.wait_until) {
                (Some(x), Some(y)) => Some(x.min(y)),
                (x, y) => x.or(y),
            };
        }
    }

    // Prefer the first feasible profile, but keep the previous one unless the
    // preferred one clears the minimum by the hysteresis margin.
    let keep = previous_profile.and_then(|p| feasible.iter().find(|(id, _, _)| id == p));
    let first = feasible.first();
    let pick = match (keep, first) {
        (Some(k), Some(f)) if f.0 != k.0 && f.1 >= policy.min_progress_per_hour * (1.0 + policy.switching_hysteresis) => {
            Some(f)
        }
        (Some(k), _) => Some(k),
        (None, f) => f,
    };
    let choice = match (pick, &local, &slow) {
        (Some((id, rate, unbounded)), _, _) => {
            Choice::Cloud { profile_id: id.clone(), rate_per_hour: *rate, unbounded_windows: unbounded.clone() }
        }
        (None, Some(id), _) => Choice::Local { profile_id: id.clone() },
        (None, None, Some((id, rate, unbounded))) => {
            Choice::Cloud { profile_id: id.clone(), rate_per_hour: *rate, unbounded_windows: unbounded.clone() }
        }
        (None, None, None) if policy_wait => Choice::Wait { until: wait_until },
        (None, None, None) if !exhausted.is_empty() => Choice::ProviderExhausted { windows: exhausted, until: wait_until },
        (None, None, None) if !blocked.is_empty() => Choice::ObservationBlocked { windows: blocked },
        (None, None, None) => Choice::NoBackend,
    };
    PolicyDecision {
        decision_id: EntityId::new(),
        policy_id: policy.policy_id,
        policy_revision: policy.revision,
        based_on_forecasts: forecasts.iter().map(|f| f.forecast_id).collect(),
        decided_at: now,
        valid_until: now + policy.decision_ttl,
        choice,
        considered,
    }
}
