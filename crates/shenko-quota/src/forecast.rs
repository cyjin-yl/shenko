//! Rolling planning baseline (§42.5): per window, the budget that can still
//! be planned before its period ends and the steady rate that spends it.
//! Every quantity stays in its window's unit; whatever cannot be determined
//! makes the result unknown instead of a guessed number.
use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
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

/// One dispatched attempt charged against a window, in that window's unit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalAttempt {
    pub request_id: EntityId,
    /// Local ledger position, compared against a snapshot's `covered_through`.
    pub ledger_seq: u64,
    pub window_key: String,
    pub unit: String,
    pub cost: Range,
    pub billing: AttemptBilling,
}

impl LocalAttempt {
    fn charge(&self) -> Range {
        match self.billing {
            AttemptBilling::Settled => self.cost,
            AttemptBilling::Unknown => Range::new(0.0, self.cost.upper),
        }
    }
}

/// Planning inputs per window, in the window's unit.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowAssumptions {
    pub reserve: f64,
    pub margin: f64,
    /// Expected consumption by other clients until the period ends. Needed
    /// whenever the account is not exclusive; absent means unknown.
    pub external: Option<Range>,
}

impl Default for WindowAssumptions {
    fn default() -> Self {
        Self { reserve: 0.0, margin: 0.0, external: None }
    }
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
    pub planning: Planning,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaForecast {
    pub forecast_id: EntityId,
    pub based_on_snapshot: EntityId,
    pub account_scope_ref: String,
    pub computed_at: DateTime<Utc>,
    pub freshness: Freshness,
    pub windows: Vec<WindowForecast>,
    /// Expected windows with no reading: unknown, never full or exhausted.
    pub unobserved: Vec<AbsentWindow>,
}

/// Own usage the snapshot does not yet reflect. With a coverage proof only
/// later attempts count; without one the overlap stays open (from nothing
/// to everything), so neither double counting nor premature settling.
fn unreflected(snapshot: &QuotaSnapshot, window: &QuotaWindow, attempts: &[LocalAttempt]) -> Option<Range> {
    let mut total = Range::zero();
    for attempt in attempts.iter().filter(|a| a.window_key == window.window_key) {
        if attempt.unit != window.unit {
            return None;
        }
        match snapshot.covered_through {
            Some(c) if attempt.ledger_seq <= c.local_ledger_seq => {}
            Some(_) => total = total.add(attempt.charge()),
            None => total = total.add(Range::new(0.0, attempt.charge().upper)),
        }
    }
    Some(total)
}

pub fn forecast(
    snapshot: &QuotaSnapshot,
    attempts: &[LocalAttempt],
    assumptions: &HashMap<String, WindowAssumptions>,
    now: DateTime<Utc>,
    ttl: chrono::Duration,
) -> QuotaForecast {
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
            let planning = plan(window, remaining, own, external, assume, now);
            WindowForecast {
                window_key: window.window_key.clone(),
                unit: window.unit.clone(),
                remaining,
                unreflected_own_cost: own,
                external_cost: external,
                planning,
            }
        })
        .collect();
    QuotaForecast {
        forecast_id: EntityId::new(),
        based_on_snapshot: snapshot.snapshot_id,
        account_scope_ref: snapshot.account_scope_ref.clone(),
        computed_at: now,
        freshness: snapshot.freshness(now, ttl),
        windows,
        unobserved: snapshot.absent_windows.clone(),
    }
}

fn unknown(reason: &str) -> Planning {
    Planning::Unknown { reason: reason.into() }
}

fn plan(
    window: &QuotaWindow,
    remaining: Option<Range>,
    own: Option<Range>,
    external: Option<Range>,
    assume: WindowAssumptions,
    now: DateTime<Utc>,
) -> Planning {
    if window.kind == WindowKind::TokenBucket {
        return match (remaining, own) {
            (Some(r), Some(o)) => Planning::Replenishing {
                remaining: Range::new((r.lower - o.upper).max(0.0), (r.upper - o.lower).max(0.0)),
                refill_by: window.replenishes_by,
            },
            _ => unknown("bucket remaining or own usage unknown"),
        };
    }
    let Some(resets_at) = window.resets_at else {
        return unknown("no period end observed");
    };
    let hours = (resets_at - now).num_seconds() as f64 / 3600.0;
    if hours <= 0.0 {
        return unknown("period ended; awaiting a new observation");
    }
    let Some(r) = remaining else { return unknown("remaining unknown") };
    let Some(u) = own else { return unknown("own usage in this unit unknown") };
    let Some(e) = external else { return unknown("shared account without an external usage assumption") };
    let available = (r.lower - u.upper - e.upper - assume.reserve - assume.margin).max(0.0);
    Planning::Period { available, per_hour: available / hours, resets_at }
}
