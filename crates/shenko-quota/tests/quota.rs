//! QA01–QA03 observation/forecast behavior on documented field shapes.
//! Fixtures follow the Claude Code statusline and API rate-limit header
//! formats; no live account is probed.
use std::collections::HashMap;

use chrono::{Duration, TimeZone, Utc};
use shenko_quota::*;
use shenko_types::ids::EntityId;

fn ctx(account: &str, attribution: Attribution) -> ObservationContext {
    ObservationContext {
        provider_id: "anthropic".into(),
        account_scope_ref: account.into(),
        source_version: Some("2.1.290".into()),
        attribution,
        observed_at: Utc.with_ymd_and_hms(2026, 10, 27, 12, 0, 0).unwrap(),
    }
}

/// QA01: a subscription percentage and API token buckets stay separate
/// windows with their own units, scopes and reset meanings.
#[test]
fn subscription_percent_and_api_buckets_stay_separate() {
    let now = ctx("sub", Attribution::Exclusive).observed_at;
    let reset = (now + Duration::days(3)).timestamp();
    let statusline = format!(
        r#"{{"model":{{"id":"x"}},"rate_limits":{{"five_hour":{{"used_percentage":40,"resets_at":{}}},"seven_day":{{"used_percentage":55.5,"resets_at":{reset}}}}}}}"#,
        (now + Duration::hours(2)).timestamp()
    );
    let sub = from_statusline(&ctx("sub", Attribution::Exclusive), &statusline, true);
    let api = from_api_headers(
        &ctx("api-key", Attribution::Exclusive),
        [
            ("Anthropic-RateLimit-Input-Tokens-Limit", "40000"),
            ("anthropic-ratelimit-input-tokens-remaining", "39000"),
            ("anthropic-ratelimit-input-tokens-reset", "2026-10-27T12:00:30Z"),
            ("x-api-key", "sk-secret"),
            ("authorization", "Bearer secret"),
        ],
    );

    let week = sub.window("seven_day").unwrap();
    assert_eq!((week.metric, week.unit.as_str(), week.kind), (Metric::UsagePercent, "percent", WindowKind::Fixed));
    assert_eq!(week.remaining_range(), Some(Range::point(44.5)));
    let bucket = api.window("api:input-tokens").unwrap();
    assert_eq!((bucket.metric, bucket.kind), (Metric::InputTokens, WindowKind::TokenBucket));
    assert!(bucket.resets_at.is_none(), "a refill time is not a period reset");
    assert!(bucket.replenishes_by.is_some());
    assert!(!api.raw_evidence.contains("secret"), "credentials never enter evidence");
    // Headers that were not sent are unsupported, not unlimited.
    assert!(api.absent_windows.iter().any(|a| a.expected_window_key == "api:requests"));

    let f = forecast(&api, &[], &HashMap::new(), now, Duration::minutes(5));
    assert!(matches!(f.windows[0].planning, Planning::Replenishing { remaining, .. } if remaining == Range::point(39000.0)));
    let f = forecast(&sub, &[], &HashMap::new(), now, Duration::minutes(5));
    let Planning::Period { available, per_hour, .. } = f.windows.iter().find(|w| w.window_key == "seven_day").unwrap().planning
    else {
        panic!("weekly window must plan over its period");
    };
    assert_eq!(available, 44.5);
    assert!((per_hour - 44.5 / 72.0).abs() < 1e-9);
}

/// QA02: before the first response, with one window, after expiry and when
/// the query itself fails, nothing reads as full or exhausted.
#[test]
fn missing_expired_and_failed_readings_stay_unknown() {
    let c = ctx("sub", Attribution::Exclusive);
    let before_first = from_statusline(&c, r#"{"model":{"id":"x"}}"#, false);
    assert_eq!(before_first.observation_status, ObservationStatus::Unavailable);
    assert!(before_first.windows.is_empty());
    assert!(before_first.absent_windows.iter().all(|a| a.reason == AbsentReason::NotYetObserved));

    let past = (c.observed_at - Duration::minutes(1)).timestamp();
    let future = (c.observed_at + Duration::hours(1)).timestamp();
    let partial = from_statusline(
        &c,
        &format!(r#"{{"rate_limits":{{"five_hour":{{"used_percentage":10,"resets_at":{future}}},"seven_day":{{"used_percentage":90,"resets_at":{past}}}}}}}"#),
        true,
    );
    assert_eq!(partial.windows.len(), 1);
    assert_eq!(
        partial.absent_windows,
        vec![AbsentWindow { expected_window_key: "seven_day".into(), reason: AbsentReason::Expired }]
    );
    let f = forecast(&partial, &[], &HashMap::new(), c.observed_at, Duration::minutes(5));
    assert_eq!(f.unobserved.len(), 1, "the expired week is unknown, not reset to full");

    let failed = observation_failed(&c, SourceKind::CliUi, &STATUSLINE_WINDOWS, "/usage returned 429");
    assert_eq!(failed.observation_status, ObservationStatus::Unavailable);
    assert!(failed.windows.is_empty() && failed.absent_windows.len() == 2);
    // Readings go stale with time; a failed read is unknown, not stale-full.
    assert_eq!(partial.freshness(c.observed_at + Duration::hours(2), Duration::minutes(5)), Freshness::Stale);
    assert_eq!(failed.freshness(c.observed_at, Duration::minutes(5)), Freshness::Unknown);
}

/// QA03: shared consumption and own attempts keep ranges; covered attempts
/// are not double counted and an unknown attempt is not refunded.
#[test]
fn shared_usage_and_unreflected_attempts_keep_ranges() {
    let c = ctx("sub", Attribution::SharedUnobserved);
    let reset = (c.observed_at + Duration::hours(10)).timestamp();
    let mut snap = from_statusline(
        &c,
        &format!(r#"{{"rate_limits":{{"five_hour":{{"used_percentage":30,"resets_at":{reset}}}}}}}"#),
        true,
    );
    let attempt = |seq, cost, billing| LocalAttempt {
        request_id: EntityId::new(),
        ledger_seq: seq,
        window_key: "five_hour".into(),
        unit: "percent".into(),
        cost,
        billing,
    };
    let attempts = vec![
        attempt(1, Range::new(2.0, 3.0), AttemptBilling::Settled),
        attempt(2, Range::new(1.0, 2.0), AttemptBilling::Settled),
        attempt(3, Range::new(4.0, 6.0), AttemptBilling::Unknown),
    ];
    let window = |f: &QuotaForecast| f.windows.iter().find(|w| w.window_key == "five_hour").unwrap().clone();
    let now = c.observed_at;
    let ttl = Duration::minutes(5);

    // Shared but no assumption about other clients: unknown, not a number.
    let f = forecast(&snap, &attempts, &HashMap::new(), now, ttl);
    assert!(matches!(window(&f).planning, Planning::Unknown { .. }));

    let assumptions = HashMap::from([(
        "five_hour".to_string(),
        WindowAssumptions { reserve: 5.0, margin: 2.0, external: Some(Range::new(0.0, 10.0)) },
    )]);
    // No coverage proof: own usage is open from 0 to the sum of upper bounds.
    let f = forecast(&snap, &attempts, &assumptions, now, ttl);
    assert_eq!(window(&f).unreflected_own_cost, Some(Range::new(0.0, 11.0)));
    // A = 70 − 11 − 10 − 5 − 2 = 42 over 10 hours.
    assert_eq!(window(&f).planning, Planning::Period {
        available: 42.0,
        per_hour: 4.2,
        resets_at: Utc.timestamp_opt(reset, 0).unwrap(),
    });

    // The report covers ledger seq ≤ 1: only seq 2 (settled) and 3 (unknown,
    // never refunded below 0) remain unreflected.
    snap.covered_through = Some(CoveredThrough { local_ledger_seq: 1 });
    let f = forecast(&snap, &attempts, &assumptions, now, ttl);
    assert_eq!(window(&f).unreflected_own_cost, Some(Range::new(1.0, 8.0)));

    // An attempt measured in tokens cannot be charged to a percent window.
    let mut mixed = attempts.clone();
    mixed.push(LocalAttempt { unit: "tokens".into(), ..attempt(4, Range::point(5000.0), AttemptBilling::Settled) });
    let f = forecast(&snap, &mixed, &assumptions, now, ttl);
    assert_eq!(window(&f).unreflected_own_cost, None);
    assert!(matches!(window(&f).planning, Planning::Unknown { .. }));
}
