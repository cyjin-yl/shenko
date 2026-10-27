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
            (_, Some(lower), Some(upper)) => (lower, upper),
            (Some(value), _, _) => (value, value),
            _ => return None,
        };
        let range = Range::checked(lower, upper)?;
        Some(Range { lower: (range.lower - pad).max(0.0), upper: range.upper + pad })
    }
}

/// A closed interval in one unit.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Range {
    pub lower: f64,
    pub upper: f64,
}

impl Range {
    pub fn new(lower: f64, upper: f64) -> Self {
        Self { lower: lower.min(upper), upper: upper.max(lower) }
    }

    /// A non-negative finite interval with `lower <= upper`, else None.
    pub fn checked(lower: f64, upper: f64) -> Option<Self> {
        let valid = lower.is_finite() && upper.is_finite() && lower >= 0.0 && lower <= upper;
        valid.then_some(Self { lower, upper })
    }

    pub fn point(value: f64) -> Self {
        Self { lower: value, upper: value }
    }

    pub fn zero() -> Self {
        Self::point(0.0)
    }

    pub fn add(self, other: Range) -> Range {
        Range::new(self.lower + other.lower, self.upper + other.upper)
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
    /// Remaining quota as an interval: reported remaining, else capacity − used.
    pub fn remaining_range(&self) -> Option<Range> {
        if let Some(remaining) = self.remaining.and_then(|m| m.range()) {
            return Some(remaining);
        }
        let capacity = self.capacity.and_then(|m| m.range())?;
        let used = self.used.and_then(|m| m.range())?;
        if used.lower > capacity.upper {
            return None;
        }
        Some(Range::new((capacity.lower - used.upper).max(0.0), capacity.upper - used.lower))
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
    pub observation_error: Option<String>,
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
