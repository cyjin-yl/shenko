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

/// A read that failed (e.g. `/usage` itself rate limited, a timeout). The
/// quota is unknown, not exhausted; it must not deny inference admission.
pub fn observation_failed(
    ctx: &ObservationContext,
    source_kind: SourceKind,
    expected_windows: &[&str],
    error: impl Into<String>,
) -> QuotaSnapshot {
    let mut s = snapshot(ctx, source_kind, String::new());
    s.observation_status = ObservationStatus::Unavailable;
    s.observation_error = Some(error.into());
    s.absent_windows = expected_windows
        .iter()
        .map(|key| AbsentWindow { expected_window_key: (*key).into(), reason: AbsentReason::Unknown })
        .collect();
    s
}

/// Parse the statusline JSON Claude Code passes to a status command.
///
/// `rate_limits` only appears for applicable Pro/Max logins after the first
/// API response; `first_response_seen` tells whether its absence can still
/// mean "not yet observed". A window whose `resets_at` has passed is expired:
/// its old reading is dropped, and nothing about the new period is assumed.
pub fn from_statusline(ctx: &ObservationContext, json: &str, first_response_seen: bool) -> QuotaSnapshot {
    let Ok(root) = serde_json::from_str::<Value>(json) else {
        return observation_failed(ctx, SourceKind::CliStatusline, &STATUSLINE_WINDOWS, "statusline JSON did not parse");
    };
    let rate_limits = root.get("rate_limits").filter(|v| v.is_object());
    let raw = rate_limits.map(Value::to_string).unwrap_or_default();
    let mut s = snapshot(ctx, SourceKind::CliStatusline, raw);
    let missing = if first_response_seen { AbsentReason::Unknown } else { AbsentReason::NotYetObserved };
    for key in STATUSLINE_WINDOWS {
        let entry = rate_limits.and_then(|r| r.get(key));
        let used = entry.and_then(|e| e.get("used_percentage")).and_then(Value::as_f64);
        let resets_at = entry
            .and_then(|e| e.get("resets_at"))
            .and_then(Value::as_i64)
            .and_then(|secs| Utc.timestamp_opt(secs, 0).single());
        let reason = match (used, resets_at) {
            (Some(_), Some(reset)) if reset <= ctx.observed_at => Some(AbsentReason::Expired),
            (Some(_), _) => None,
            _ => Some(missing),
        };
        if let Some(reason) = reason {
            s.absent_windows.push(AbsentWindow { expected_window_key: key.into(), reason });
            continue;
        }
        s.windows.push(QuotaWindow {
            window_key: key.into(),
            scope_ref: "subscription".into(),
            kind: WindowKind::Fixed,
            metric: Metric::UsagePercent,
            unit: "percent".into(),
            used: used.map(Measurement::reported),
            remaining: None,
            capacity: Some(Measurement::reported(100.0)),
            resets_at,
            replenishes_by: None,
            enforcement: Enforcement::ProviderHardLimit,
        });
    }
    if s.windows.is_empty() && rate_limits.is_none() {
        s.observation_status = ObservationStatus::Unavailable;
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

/// Parse API rate-limit response headers. These are token buckets refilled
/// continuously: `*-reset` is when the bucket is full again, never an
/// account period end, and the limits are not guaranteed capacity. Only the
/// rate-limit headers are kept as evidence; any other header (credentials
/// included) is ignored.
pub fn from_api_headers<'a>(
    ctx: &ObservationContext,
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> QuotaSnapshot {
    let kept: Vec<(String, String)> = headers
        .into_iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .filter(|(k, _)| k.starts_with(HEADER_PREFIX) || k == "retry-after")
        .collect();
    let raw = kept.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\n");
    let mut s = snapshot(ctx, SourceKind::ApiHeaders, raw);
    let get = |name: String| kept.iter().find(|(k, _)| *k == name).map(|(_, v)| v.as_str());
    for (resource, metric, unit) in HEADER_RESOURCES {
        let field = |suffix: &str| get(format!("{HEADER_PREFIX}{resource}-{suffix}"));
        let limit = field("limit").and_then(|v| v.parse::<f64>().ok());
        let remaining = field("remaining").and_then(|v| v.parse::<f64>().ok());
        let reset = field("reset")
            .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
            .map(|t| t.with_timezone(&Utc));
        if limit.is_none() && remaining.is_none() {
            s.absent_windows.push(AbsentWindow {
                expected_window_key: format!("api:{resource}"),
                reason: AbsentReason::Unsupported,
            });
            continue;
        }
        s.windows.push(QuotaWindow {
            window_key: format!("api:{resource}"),
            scope_ref: "api_key".into(),
            kind: WindowKind::TokenBucket,
            metric,
            unit: unit.into(),
            used: None,
            remaining: remaining.map(Measurement::reported),
            capacity: limit.map(Measurement::reported),
            resets_at: None,
            replenishes_by: reset,
            enforcement: Enforcement::ProviderHardLimit,
        });
    }
    if s.windows.is_empty() {
        s.observation_status = ObservationStatus::Unsupported;
    }
    s
}
