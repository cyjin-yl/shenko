//! Non-model observers (§42.2). They read what a provider already exposes on
//! normal traffic; none of them starts a paid generation to probe a balance.
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use shenko_types::ids::EntityId;

use crate::model::*;

/// Windows the Claude Code statusline may report. Each can be absent
/// independently; absence is never 0% or 100%.
pub const STATUSLINE_WINDOWS: [&str; 2] = ["five_hour", "seven_day"];

/// What the caller knows about the account behind a reading.
#[derive(Debug, Clone)]
pub struct ObservationContext {
    pub provider_id: String,
    pub account_scope_ref: String,
    pub source_version: Option<String>,
    pub attribution: Attribution,
    pub observed_at: DateTime<Utc>,
}

fn snapshot(ctx: &ObservationContext, source_kind: SourceKind, raw_evidence: String) -> QuotaSnapshot {
    QuotaSnapshot {
        snapshot_id: EntityId::new(),
        provider_id: ctx.provider_id.clone(),
        account_scope_ref: ctx.account_scope_ref.clone(),
        source_kind,
        source_version: ctx.source_version.clone(),
        raw_evidence,
        observed_at: ctx.observed_at,
        observation_status: ObservationStatus::Available,
        observation_error: None,
        attribution: ctx.attribution,
        covered_through: None,
        windows: Vec::new(),
        absent_windows: Vec::new(),
    }
}

fn absent(key: impl Into<String>, reason: AbsentReason) -> AbsentWindow {
    AbsentWindow { expected_window_key: key.into(), reason }
}

/// A read that failed (e.g. `/usage` itself rate limited, a timeout). The
/// quota is unknown, not exhausted; it must not deny inference admission.
/// Only a typed classification is retained, never a raw error or response.
pub fn observation_failed(
    ctx: &ObservationContext,
    source_kind: SourceKind,
    expected_windows: &[&str],
    error: ObservationError,
) -> QuotaSnapshot {
    let mut s = snapshot(ctx, source_kind, String::new());
    s.observation_status = ObservationStatus::Unavailable;
    s.observation_error = Some(error);
    s.absent_windows = expected_windows.iter().map(|key| absent(*key, AbsentReason::Unknown)).collect();
    s
}

/// Parse the statusline JSON Claude Code passes to a status command.
///
/// `rate_limits` only appears for applicable Pro/Max logins after the first
/// API response; `first_response_seen` tells whether its absence can still
/// mean "not yet observed". A percentage outside 0–100 or non-numeric is
/// invalid, not clamped. A window whose `resets_at` has passed is expired:
/// its old reading is dropped, and nothing about the new period is assumed.
/// Evidence keeps only the parsed window values.
pub fn from_statusline(ctx: &ObservationContext, json: &str, first_response_seen: bool) -> QuotaSnapshot {
    let Ok(root) = serde_json::from_str::<Value>(json) else {
        return observation_failed(ctx, SourceKind::CliStatusline, &STATUSLINE_WINDOWS, ObservationError::InvalidData);
    };
    let rate_limits = root.get("rate_limits").filter(|v| v.is_object());
    let mut s = snapshot(ctx, SourceKind::CliStatusline, String::new());
    let missing = if first_response_seen { AbsentReason::Unknown } else { AbsentReason::NotYetObserved };
    let mut evidence = Vec::new();
    for key in STATUSLINE_WINDOWS {
        let Some(entry) = rate_limits.and_then(|r| r.get(key)) else {
            s.absent_windows.push(absent(key, missing));
            continue;
        };
        let used = entry.get("used_percentage").and_then(Value::as_f64);
        let resets_at = match entry.get("resets_at") {
            None => Ok(None),
            Some(v) => v.as_i64().and_then(|secs| Utc.timestamp_opt(secs, 0).single()).map(Some).ok_or(()),
        };
        let (used, resets_at) = match (used, resets_at) {
            (Some(u), Ok(r)) if u.is_finite() && (0.0..=100.0).contains(&u) => (u, r),
            _ => {
                s.absent_windows.push(absent(key, AbsentReason::Invalid));
                continue;
            }
        };
        if resets_at.is_some_and(|r| r <= ctx.observed_at) {
            s.absent_windows.push(absent(key, AbsentReason::Expired));
            continue;
        }
        evidence.push(format!(
            "{key}.used_percentage={used} {key}.resets_at={}",
            resets_at.map_or("absent".into(), |r| r.timestamp().to_string())
        ));
        s.windows.push(QuotaWindow {
            window_key: key.into(),
            scope_ref: "subscription".into(),
            kind: WindowKind::Fixed,
            metric: Metric::UsagePercent,
            unit: "percent".into(),
            used: Some(Measurement::reported(used)),
            remaining: None,
            capacity: Some(Measurement::reported(100.0)),
            resets_at,
            replenishes_by: None,
            enforcement: Enforcement::ProviderHardLimit,
        });
    }
    s.raw_evidence = evidence.join("\n");
    if s.windows.is_empty() {
        if s.absent_windows.iter().any(|a| a.reason == AbsentReason::Invalid) {
            s.observation_status = ObservationStatus::Unavailable;
            s.observation_error = Some(ObservationError::InvalidData);
        } else if rate_limits.is_none() {
            s.observation_status = ObservationStatus::Unavailable;
        }
    }
    s
}

const HEADER_PREFIX: &str = "anthropic-ratelimit-";
const HEADER_RESOURCES: [(&str, Metric, &str); 4] = [
    ("requests", Metric::Requests, "requests"),
    ("tokens", Metric::Tokens, "tokens"),
    ("input-tokens", Metric::InputTokens, "tokens"),
    ("output-tokens", Metric::OutputTokens, "tokens"),
];
const HEADER_FIELDS: [&str; 3] = ["limit", "remaining", "reset"];

/// Enclose the exact integer plus any documented provider rounding, even when
/// those values cannot be represented exactly by f64.
fn count_measurement(value: u64, half_width: u64) -> Measurement {
    let bounds = |value: u128| {
        let rounded = value as f64;
        match (rounded as u128).cmp(&value) {
            std::cmp::Ordering::Equal => (rounded, rounded),
            std::cmp::Ordering::Less => (rounded, rounded.next_up()),
            std::cmp::Ordering::Greater => (rounded.next_down(), rounded),
        }
    };
    let lower = bounds(u128::from(value.saturating_sub(half_width))).0;
    let upper = bounds(u128::from(value) + u128::from(half_width)).1;
    if lower == upper { return Measurement::reported(lower); }
    Measurement { value: None, lower: Some(lower), upper: Some(upper),
        basis: Basis::ProviderReported, precision: None }
}

/// The single value of an allowlisted header. Repeats that disagree make the
/// field unusable instead of picking one.
enum Field<'a> {
    Missing,
    One(&'a str),
    Conflict,
}

fn field<'a>(headers: &'a [(String, String)], name: &str) -> Field<'a> {
    let mut values = headers.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str());
    match values.next() {
        None => Field::Missing,
        Some(first) if values.all(|v| v == first) => Field::One(first),
        Some(_) => Field::Conflict,
    }
}

/// Parse API rate-limit response headers. These are token buckets refilled
/// continuously: `*-reset` is when the bucket is full again, never an
/// account period end, and the limits are not guaranteed capacity.
///
/// Only the exact `anthropic-ratelimit-{resource}-{limit,remaining,reset}`
/// names are read; any other header, credentials and unknown extensions
/// included, is ignored. Counts are non-negative integers. Documented token
/// remaining is rounded to the nearest thousand, so its conservative range
/// must overlap the limit; request remaining has no documented rounding here.
/// Evidence holds parsed values and rounding basis, never header text.
pub fn from_api_headers<'a>(
    ctx: &ObservationContext,
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> QuotaSnapshot {
    let allowed: Vec<String> = HEADER_RESOURCES
        .iter()
        .flat_map(|(resource, _, _)| HEADER_FIELDS.iter().map(move |f| format!("{HEADER_PREFIX}{resource}-{f}")))
        .collect();
    let kept: Vec<(String, String)> = headers
        .into_iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .filter(|(k, _)| allowed.contains(k))
        .collect();
    let mut s = snapshot(ctx, SourceKind::ApiHeaders, String::new());
    let mut evidence = Vec::new();
    for (resource, metric, unit) in HEADER_RESOURCES {
        let key = format!("api:{resource}");
        // Anthropic documents token-remaining headers as nearest-thousand;
        // request counts and the advertised limits are not documented so.
        let remaining_half_width = if metric == Metric::Requests { 0 } else { 500 };
        let get = |suffix: &str| field(&kept, &format!("{HEADER_PREFIX}{resource}-{suffix}"));
        let count = |f: Field| match f {
            Field::Missing => Ok(None),
            Field::One(v) => v.parse::<u64>().map(Some).map_err(|_| ()),
            Field::Conflict => Err(()),
        };
        let reset = match get("reset") {
            Field::Missing => Ok(None),
            Field::One(v) => DateTime::parse_from_rfc3339(v).map(|t| Some(t.with_timezone(&Utc))).map_err(|_| ()),
            Field::Conflict => Err(()),
        };
        let (limit, remaining, reset) = match (count(get("limit")), count(get("remaining")), reset) {
            (Ok(None), Ok(None), Ok(None)) => {
                s.absent_windows.push(absent(key, AbsentReason::Unsupported));
                continue;
            }
            (Ok(None), Ok(None), Ok(Some(_))) => {
                s.absent_windows.push(absent(key, AbsentReason::Unknown));
                continue;
            }
            (Ok(limit), Ok(remaining), Ok(reset))
                if limit.zip(remaining).is_none_or(|(limit, remaining)|
                    remaining.saturating_sub(remaining_half_width) <= limit) =>
            {
                (limit, remaining, reset)
            }
            _ => {
                s.absent_windows.push(absent(key, AbsentReason::Invalid));
                continue;
            }
        };
        let show = |v: Option<u64>| v.map_or("absent".to_string(), |n| n.to_string());
        evidence.push(format!(
            "{key} limit={} remaining={} remaining_rounding_half_width={} reset={}",
            show(limit),
            show(remaining),
            remaining_half_width,
            reset.map_or("absent".into(), |r| r.to_rfc3339())
        ));
        s.windows.push(QuotaWindow {
            window_key: key,
            scope_ref: "api_key".into(),
            kind: WindowKind::TokenBucket,
            metric,
            unit: unit.into(),
            used: None,
            remaining: remaining.map(|value| count_measurement(value, remaining_half_width)),
            capacity: limit.map(|value| count_measurement(value, 0)),
            resets_at: None,
            replenishes_by: reset,
            enforcement: Enforcement::ProviderHardLimit,
        });
    }
    s.raw_evidence = evidence.join("\n");
    if s.windows.is_empty() {
        if s.absent_windows.iter().any(|a| a.reason == AbsentReason::Invalid) {
            s.observation_status = ObservationStatus::Unavailable;
            s.observation_error = Some(ObservationError::InvalidData);
        } else if s.absent_windows.iter().any(|a| a.reason != AbsentReason::Unsupported) {
            s.observation_status = ObservationStatus::Unavailable;
        } else {
            s.observation_status = ObservationStatus::Unsupported;
        }
    }
    s
}
