//! Rolling planning baseline (§42.5): per window, the budget that can still
//! be planned before its period ends and the steady rate that spends it.
//! Every quantity stays in its window's unit and account; whatever cannot be
//! determined makes the result unknown instead of a guessed number.
use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shenko_types::commit::CommitRef;
use shenko_types::ids::EntityId;

use crate::model::*;

/// Billing state of one local request attempt, as far as we know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptBilling {
    /// Finished with a measured or estimated cost range.
    Settled,
    /// Armed, disconnected or cancelled after send: may have cost anything up
    /// to its upper bound, and is never refunded just because we stopped it.
    Unknown,
}

/// One dispatched attempt charged against one provider window of one
/// account, in that window's unit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalAttempt {
    pub request_id: EntityId,
    pub provider_id: String,
    pub account_scope_ref: String,
    pub window_key: String,
    pub unit: String,
    /// Where the attempt was committed locally; only comparable with a
    /// coverage proof from the same ledger.
    pub ledger_commit: CommitRef,
    pub cost: Range,
    pub billing: AttemptBilling,
}

impl LocalAttempt {
    fn charge(&self) -> Option<Range> {
        let cost = Range::new(self.cost.lower, self.cost.upper)?;
        match self.billing {
            AttemptBilling::Settled => Some(cost),
            AttemptBilling::Unknown => Range::new(0.0, cost.upper),
        }
    }
}

/// Planning inputs per window, in the window's unit.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowAssumptions {
    pub reserve: f64,
    pub margin: f64,
    /// Expected consumption by other clients over the caller's planning
    /// horizon. Needed for any shared window; absent means unknown.
    pub external: Option<Range>,
}

impl Default for WindowAssumptions {
    fn default() -> Self {
        Self { reserve: 0.0, margin: 0.0, external: None }
    }
}

impl WindowAssumptions {
    fn valid(&self) -> bool {
        let amount = |v: f64| v.is_finite() && v >= 0.0;
        amount(self.reserve)
            && amount(self.margin)
            && self.external.is_none_or(|e| Range::new(e.lower, e.upper).is_some())
    }
}

/// How a reading that is no longer fresh may be used. The choice belongs to
/// the caller's policy; the forecast records which one was applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleUse {
    /// Stale readings plan nothing.
    Refuse,
    /// Plan from the last known reading, marked as stale.
    LastKnown,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ForecastOptions {
    /// How long a reading from this source stays current.
    pub ttl: chrono::Duration,
    pub stale: StaleUse,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Planning {
    /// A_j and r_j = A_j / T_j for a window with an observed period end.
    Period { available: f64, per_hour: f64, resets_at: DateTime<Utc> },
    /// A token bucket: spend what is there, it refills continuously. No
    /// period formula applies.
    Replenishing { remaining: Range, refill_by: Option<DateTime<Utc>> },
    Unknown { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowForecast {
    pub window_key: String,
    pub unit: String,
    pub remaining: Option<Range>,
    pub unreflected_own_cost: Option<Range>,
    pub external_cost: Option<Range>,
    /// Freshness of the reading this window was planned from.
    pub basis: Freshness,
    pub planning: Planning,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaForecast {
    pub forecast_id: EntityId,
    pub based_on_snapshot: EntityId,
    pub provider_id: String,
    pub account_scope_ref: String,
    pub computed_at: DateTime<Utc>,
    pub freshness: Freshness,
    pub windows: Vec<WindowForecast>,
    /// Expected windows with no usable reading: unknown, never full or exhausted.
    pub unobserved: Vec<AbsentWindow>,
}

/// Own usage of this account's window that the snapshot does not reflect.
/// A coverage proof only settles attempts committed in the same ledger;
/// for any other attempt the overlap stays open (from nothing to its upper
/// bound), so neither double counting nor premature settling.
fn unreflected(snapshot: &QuotaSnapshot, window: &QuotaWindow, attempts: &[LocalAttempt]) -> Option<Range> {
    let mut total = Range::zero();
    let mine = attempts.iter().filter(|a| {
        a.provider_id == snapshot.provider_id
            && a.account_scope_ref == snapshot.account_scope_ref
            && a.window_key == window.window_key
    });
    for attempt in mine {
        if attempt.unit != window.unit {
            return None;
        }
        let charge = attempt.charge()?;
        let proof = snapshot.covered_through.map(|c| c.local_ledger_commit);
        match proof {
            Some(c) if c.ledger_id == attempt.ledger_commit.ledger_id => {
                if attempt.ledger_commit.seq > c.seq {
                    total = total.checked_add(charge)?;
                }
            }
            _ => total = total.checked_add(Range::new(0.0, charge.upper)?)?,
        }
    }
    Some(total)
}

pub fn forecast(
    snapshot: &QuotaSnapshot,
    attempts: &[LocalAttempt],
    assumptions: &HashMap<String, WindowAssumptions>,
    now: DateTime<Utc>,
    options: ForecastOptions,
) -> QuotaForecast {
    let freshness = snapshot.freshness(now, options.ttl);
    let windows = snapshot
        .windows
        .iter()
        .map(|window| {
            let assume = assumptions.get(&window.window_key).copied().unwrap_or_default();
            let remaining = window.remaining_range();
            let own = unreflected(snapshot, window, attempts);
            let external = match snapshot.attribution {
                Attribution::Exclusive => Some(Range::zero()),
                _ => assume.external,
            };
            let planning = match freshness {
                Freshness::Fresh => plan(window, remaining, own, external, assume, now),
                Freshness::Stale if options.stale == StaleUse::LastKnown => {
                    plan(window, remaining, own, external, assume, now)
                }
                Freshness::Stale => unknown("reading is stale and the policy refuses stale readings"),
                Freshness::Unknown => unknown("reading time cannot be trusted at now"),
            };
            WindowForecast {
                window_key: window.window_key.clone(),
                unit: window.unit.clone(),
                remaining,
                unreflected_own_cost: own,
                external_cost: external,
                basis: freshness,
                planning,
            }
        })
        .collect();
    QuotaForecast {
        forecast_id: EntityId::new(),
        based_on_snapshot: snapshot.snapshot_id,
        provider_id: snapshot.provider_id.clone(),
        account_scope_ref: snapshot.account_scope_ref.clone(),
        computed_at: now,
        freshness,
        windows,
        unobserved: snapshot.absent_windows.clone(),
    }
}

fn unknown(reason: &str) -> Planning {
    Planning::Unknown { reason: reason.into() }
}

fn net_budget(remaining: Range, own: Range, external: Range, assume: WindowAssumptions) -> Option<Range> {
    remaining.checked_sub(own)?
        .checked_sub(external)?
        .checked_sub(Range::point(assume.reserve)?)?
        .checked_sub(Range::point(assume.margin)?)
}

fn plan(
    window: &QuotaWindow,
    remaining: Option<Range>,
    own: Option<Range>,
    external: Option<Range>,
    assume: WindowAssumptions,
    now: DateTime<Utc>,
) -> Planning {
    if !assume.valid() {
        return unknown("invalid planning assumption");
    }
    match window.kind {
        WindowKind::TokenBucket => {
            return match (remaining, own, external) {
                (Some(r), Some(o), Some(e)) => match net_budget(r, o, e, assume) {
                    Some(remaining) => Planning::Replenishing { remaining, refill_by: window.replenishes_by },
                    None => unknown("bucket arithmetic overflow or inconsistent interval"),
                },
                _ => unknown("bucket remaining, own usage, or shared external usage unknown"),
            };
        }
        WindowKind::Fixed | WindowKind::Rolling | WindowKind::Calendar => {}
        WindowKind::Unknown => return unknown("window kind unknown; no period formula applies"),
    }
    let Some(resets_at) = window.resets_at else {
        return unknown("no period end observed");
    };
    let duration = resets_at - now;
    if duration <= chrono::Duration::zero() {
        return unknown("period ended; awaiting a new observation");
    }
    // An upper time bound keeps the resulting spending rate conservative;
    // preserve subsecond duration instead of truncating it to whole seconds.
    let fraction = duration.subsec_nanos() as f64 / 1_000_000_000.0;
    let fraction_upper = if fraction > 0.0 { fraction.next_up() } else { 0.0 };
    let Some(seconds_upper) = outward_sum(duration.num_seconds() as f64, fraction_upper, true) else {
        return unknown("period duration cannot be represented");
    };
    let hours = (seconds_upper / 3600.0).next_up();
    let Some(r) = remaining else { return unknown("remaining unknown") };
    let Some(u) = own else { return unknown("own usage in this unit unknown") };
    let Some(e) = external else { return unknown("shared account without an external usage assumption") };
    let Some(net) = net_budget(r, u, e, assume) else { return unknown("period planning arithmetic overflow") };
    let available = net.lower;
    let rate = available / hours;
    let per_hour = if rate > 0.0 && rate.is_finite() { rate.next_down() } else { rate };
    if !available.is_finite() || !per_hour.is_finite() {
        return unknown("period planning arithmetic overflow");
    }
    Planning::Period { available, per_hour, resets_at }
}
