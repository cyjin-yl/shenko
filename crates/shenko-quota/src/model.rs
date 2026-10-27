//! Normative quota types (§42.3). Identifiers are stable within an account
//! namespace; `account_scope_ref` names a secret, it never holds one.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shenko_types::commit::CommitRef;
use shenko_types::ids::EntityId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    ApiHeaders,
    AdminApi,
    CliStatusline,
    CliUi,
    Manual,
    LocalMeter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Fresh,
    Stale,
    Unknown,
}

/// Whether the observation itself worked. `Unavailable` is a failed or
/// not-yet-possible read, never a statement about the remaining quota.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationStatus {
    Available,
    Unavailable,
    Unsupported,
}

/// A bounded failure classification; never persist a raw transport error,
/// response body, URL, or credential as an observation error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationError {
    InvalidData,
    RateLimited,
    Unauthorized,
    Timeout,
    Transport,
    Other,
}

/// Who else consumes the same quota.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attribution {
    Exclusive,
    SharedKnown,
    SharedUnobserved,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    Fixed,
    Rolling,
    Calendar,
    /// Continuously refilled; its reset time is a refill time, not a period end.
    TokenBucket,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    UsagePercent,
    Requests,
    InputTokens,
    OutputTokens,
    Tokens,
    Currency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    ProviderReported,
    LocallyMeasured,
    Estimated,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Enforcement {
    ProviderHardLimit,
    ConfiguredCap,
    Informational,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbsentReason {
    NotYetObserved,
    Expired,
    Unsupported,
    /// Present but unusable: out of range, non-finite, malformed or conflicting.
    Invalid,
    Unknown,
}

/// A value with its provenance. `value` without bounds is a point; bounds
/// without `value` are an interval; all empty means unknown.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    pub value: Option<f64>,
    pub lower: Option<f64>,
    pub upper: Option<f64>,
    pub basis: Basis,
    /// Half-width of the provider's rounding, when known.
    pub precision: Option<f64>,
}

/// One directed bound for a finite sum. TwoSum recovers the rounding error so
/// a small precision at large magnitudes cannot silently disappear.
fn outward_sum(left: f64, right: f64, upward: bool) -> Option<f64> {
    let sum = left + right;
    if !sum.is_finite() { return None; }
    let part = sum - left;
    let error = (left - (sum - part)) + (right - part);
    if !error.is_finite() { return None; }
    let bound = if upward && error > 0.0 { sum.next_up() }
        else if !upward && error < 0.0 { sum.next_down() }
        else { sum };
    bound.is_finite().then_some(bound)
}

impl Measurement {
    pub fn reported(value: f64) -> Self {
        Self { value: Some(value), lower: None, upper: None, basis: Basis::ProviderReported, precision: None }
    }

    /// The interval this measurement allows, widened by known rounding.
    /// Quota quantities are finite and non-negative; anything else is not a
    /// usable reading and yields None rather than a clamped balance.
    pub fn range(&self) -> Option<Range> {
        let pad = self.precision.unwrap_or(0.0);
        if !pad.is_finite() || pad < 0.0 {
            return None;
        }
        let (lower, upper) = match (self.value, self.lower, self.upper) {
            (None, Some(lower), Some(upper)) => (lower, upper),
            (Some(value), None, None) => (value, value),
            (Some(value), Some(lower), Some(upper))
                if value.is_finite() && lower <= value && value <= upper => (lower, upper),
            _ => return None,
        };
        let range = Range::new(lower, upper)?;
        if pad == 0.0 { return Some(range); }
        let lower = if range.lower <= pad { 0.0 } else { outward_sum(range.lower, -pad, false)? };
        let upper = outward_sum(range.upper, pad, true)?;
        Range::new(lower, upper)
    }
}

/// A closed interval in one unit.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Range {
    pub lower: f64,
    pub upper: f64,
}

impl Range {
    /// A non-negative finite interval with `lower <= upper`, else None.
    pub fn new(lower: f64, upper: f64) -> Option<Self> {
        let valid = lower.is_finite() && upper.is_finite() && lower >= 0.0 && lower <= upper;
        valid.then_some(Self { lower, upper })
    }

    pub fn point(value: f64) -> Option<Self> {
        Self::new(value, value)
    }

    pub fn zero() -> Self {
        Self { lower: 0.0, upper: 0.0 }
    }

    /// Revalidate deserialized operands and reject overflow in either bound.
    pub fn checked_add(self, other: Range) -> Option<Range> {
        let left = Self::new(self.lower, self.upper)?;
        let right = Self::new(other.lower, other.upper)?;
        Self::new(left.lower + right.lower, left.upper + right.upper)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub window_key: String,
    pub scope_ref: String,
    pub kind: WindowKind,
    pub metric: Metric,
    pub unit: String,
    pub used: Option<Measurement>,
    pub remaining: Option<Measurement>,
    pub capacity: Option<Measurement>,
    /// End of a period window. Never set for a token bucket.
    pub resets_at: Option<DateTime<Utc>>,
    /// When a token bucket is fully refilled. Never a period end.
    pub replenishes_by: Option<DateTime<Utc>>,
    pub enforcement: Enforcement,
}

impl QuotaWindow {
    /// Remaining quota, reconciling any reported balance with capacity/usage.
    pub fn remaining_range(&self) -> Option<Range> {
        let checked = |value: Option<Measurement>| -> Option<Option<Range>> {
            match value { Some(value) => value.range().map(Some), None => Some(None) }
        };
        let remaining = checked(self.remaining)?;
        let capacity = checked(self.capacity)?;
        let used = checked(self.used)?;
        let derived = match (capacity, used) {
            (Some(capacity), Some(used)) => {
                if used.lower > capacity.upper { return None; }
                Some(Range::new((capacity.lower - used.upper).max(0.0), capacity.upper - used.lower)?)
            }
            _ => None,
        };
        match remaining {
            Some(remaining) => {
                let mut upper = remaining.upper;
                if let Some(capacity) = capacity {
                    if remaining.lower > capacity.upper { return None; }
                    upper = upper.min(capacity.upper);
                }
                let lower = derived.map_or(remaining.lower, |range| remaining.lower.max(range.lower));
                upper = derived.map_or(upper, |range| upper.min(range.upper));
                Range::new(lower, upper)
            }
            None => derived,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbsentWindow {
    pub expected_window_key: String,
    pub reason: AbsentReason,
}

/// Proof of how far the provider's figures already include local usage.
/// Sequence numbers are only comparable within one ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoveredThrough {
    /// Attempts committed in this ledger at or before this seq are reflected.
    pub local_ledger_commit: CommitRef,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    pub snapshot_id: EntityId,
    pub provider_id: String,
    pub account_scope_ref: String,
    pub source_kind: SourceKind,
    pub source_version: Option<String>,
    /// The observed fields only (no credentials or unrelated headers).
    pub raw_evidence: String,
    pub observed_at: DateTime<Utc>,
    pub observation_status: ObservationStatus,
    pub observation_error: Option<ObservationError>,
    pub attribution: Attribution,
    pub covered_through: Option<CoveredThrough>,
    pub windows: Vec<QuotaWindow>,
    pub absent_windows: Vec<AbsentWindow>,
}

impl QuotaSnapshot {
    /// Freshness at `now` for a source whose readings stay current for `ttl`.
    /// A reading timestamped after `now` (clock skew) or a negative `ttl`
    /// cannot establish freshness: Unknown, never Fresh.
    pub fn freshness(&self, now: DateTime<Utc>, ttl: chrono::Duration) -> Freshness {
        let age = now - self.observed_at;
        match self.observation_status {
            ObservationStatus::Available if age < chrono::Duration::zero() || ttl < chrono::Duration::zero() => {
                Freshness::Unknown
            }
            ObservationStatus::Available if age <= ttl => Freshness::Fresh,
            ObservationStatus::Available => Freshness::Stale,
            _ => Freshness::Unknown,
        }
    }

    pub fn window(&self, key: &str) -> Option<&QuotaWindow> {
        self.windows.iter().find(|w| w.window_key == key)
    }
}
