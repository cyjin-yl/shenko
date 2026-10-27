//! QA01–QA03 observation/forecast behavior on documented field shapes.
//! Fixtures follow the Claude Code statusline and API rate-limit header
//! formats; no live account is probed.
use std::collections::HashMap;

use chrono::{Duration, TimeZone, Utc};
use shenko_quota::*;
use shenko_types::commit::CommitRef;
use shenko_types::ids::{EntityId, LedgerSeq};

fn ctx(account: &str, attribution: Attribution) -> ObservationContext {
    ObservationContext {
        provider_id: "anthropic".into(),
        account_scope_ref: account.into(),
        source_version: Some("2.1.290".into()),
        attribution,
        observed_at: Utc.with_ymd_and_hms(2026, 10, 27, 12, 0, 0).unwrap(),
    }
}

fn fresh() -> ForecastOptions {
    ForecastOptions { ttl: Duration::minutes(5), stale: StaleUse::Refuse }
}

/// QA01: a subscription percentage and API token buckets stay separate
/// windows with their own units, scopes and reset meanings; header evidence
/// keeps parsed values only.
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
            // Unknown extension, credentials and malformed values never reach evidence.
            ("anthropic-ratelimit-debug-note", "token=sk-secret"),
            ("x-api-key", "sk-secret"),
            ("anthropic-ratelimit-output-tokens-limit", "sk-secret"),
            // Repeated headers that disagree are invalid, not first-wins.
            ("anthropic-ratelimit-requests-remaining", "10"),
            ("anthropic-ratelimit-requests-remaining", "900"),
        ],
    );

    let week = sub.window("seven_day").unwrap();
    assert_eq!((week.metric, week.unit.as_str(), week.kind), (Metric::UsagePercent, "percent", WindowKind::Fixed));
    assert_eq!(week.remaining_range(), Range::point(44.5));
    let bucket = api.window("api:input-tokens").unwrap();
    assert_eq!((bucket.metric, bucket.kind), (Metric::InputTokens, WindowKind::TokenBucket));
    assert!(bucket.resets_at.is_none(), "a refill time is not a period reset");
    assert!(bucket.replenishes_by.is_some());
    assert!(!api.raw_evidence.contains("secret"), "only parsed values enter evidence");
    let reason = |key: &str| api.absent_windows.iter().find(|a| a.expected_window_key == key).map(|a| a.reason);
    assert_eq!(reason("api:tokens"), Some(AbsentReason::Unsupported), "not sent is not unlimited");
    assert_eq!(reason("api:output-tokens"), Some(AbsentReason::Invalid));
    assert_eq!(reason("api:requests"), Some(AbsentReason::Invalid));

    let f = forecast(&api, &[], &HashMap::new(), now, fresh());
    assert!(matches!(f.windows[0].planning, Planning::Replenishing { remaining, .. } if Some(remaining) == Range::new(38500.0, 39500.0)));
    let f = forecast(&sub, &[], &HashMap::new(), now, fresh());
    let Planning::Period { available, per_hour, .. } = f.windows.iter().find(|w| w.window_key == "seven_day").unwrap().planning
    else {
        panic!("weekly window must plan over its period");
    };
    assert_eq!(available, 44.5);
    assert!((per_hour - 44.5 / 72.0).abs() < 1e-9);

    // A window of unknown kind is not planned as a period even with a reset.
    let mut odd = sub.clone();
    odd.windows[1].kind = WindowKind::Unknown;
    let f = forecast(&odd, &[], &HashMap::new(), now, fresh());
    assert!(matches!(f.windows[1].planning, Planning::Unknown { .. }));
}

/// QA02: before the first response, with one window, after expiry, with an
/// invalid value, a skewed clock and when the query itself fails, nothing
/// reads as full, exhausted or fresh.
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
    let f = forecast(&partial, &[], &HashMap::new(), c.observed_at, fresh());
    assert_eq!(f.unobserved.len(), 1, "the expired week is unknown, not reset to full");

    // used_percentage = -1 is invalid, never 101% remaining.
    let negative = from_statusline(
        &c,
        &format!(r#"{{"rate_limits":{{"five_hour":{{"used_percentage":-1,"resets_at":{future}}}}}}}"#),
        true,
    );
    assert!(negative.windows.is_empty());
    assert!(negative.absent_windows.contains(&AbsentWindow {
        expected_window_key: "five_hour".into(),
        reason: AbsentReason::Invalid
    }));

    let failed = observation_failed(&c, SourceKind::CliUi, &STATUSLINE_WINDOWS, ObservationError::RateLimited);
    assert_eq!(failed.observation_status, ObservationStatus::Unavailable);
    assert!(failed.windows.is_empty() && failed.absent_windows.len() == 2);
    assert_eq!(failed.freshness(c.observed_at, Duration::minutes(5)), Freshness::Unknown);

    // A reading from the future (clock skew) or a negative TTL is not fresh.
    assert_eq!(partial.freshness(c.observed_at - Duration::minutes(1), Duration::minutes(5)), Freshness::Unknown);
    assert_eq!(partial.freshness(c.observed_at, Duration::minutes(-1)), Freshness::Unknown);
    // A stale reading plans only when the caller's policy accepts last-known
    // values, and is then marked stale.
    let later = c.observed_at + Duration::minutes(30);
    let refused = forecast(&partial, &[], &HashMap::new(), later, fresh());
    assert!(matches!(refused.windows[0].planning, Planning::Unknown { .. }));
    let accepted =
        forecast(&partial, &[], &HashMap::new(), later, ForecastOptions { stale: StaleUse::LastKnown, ..fresh() });
    assert_eq!(accepted.windows[0].basis, Freshness::Stale);
    assert!(matches!(accepted.windows[0].planning, Planning::Period { .. }));
}

/// QA03: shared consumption and own attempts keep ranges; only this
/// account's attempts count, coverage only settles attempts of the same
/// ledger, and an unknown attempt is not refunded.
#[test]
fn shared_usage_and_unreflected_attempts_keep_ranges() {
    let c = ctx("sub", Attribution::SharedUnobserved);
    let reset = (c.observed_at + Duration::hours(10)).timestamp();
    let mut snap = from_statusline(
        &c,
        &format!(r#"{{"rate_limits":{{"five_hour":{{"used_percentage":30,"resets_at":{reset}}}}}}}"#),
        true,
    );
    let ledger = EntityId::new();
    let attempt = |account: &str, ledger_id, seq, cost, billing| LocalAttempt {
        request_id: EntityId::new(),
        provider_id: "anthropic".into(),
        account_scope_ref: account.into(),
        window_key: "five_hour".into(),
        unit: "percent".into(),
        ledger_commit: CommitRef::new(ledger_id, LedgerSeq(seq)),
        cost,
        billing,
    };
    let attempts = vec![
        attempt("sub", ledger, 1, Range::new(2.0, 3.0).unwrap(), AttemptBilling::Settled),
        attempt("sub", ledger, 2, Range::new(1.0, 2.0).unwrap(), AttemptBilling::Settled),
        attempt("sub", ledger, 3, Range::new(4.0, 6.0).unwrap(), AttemptBilling::Unknown),
        // Another account's five_hour window is not this one's usage.
        attempt("other-account", ledger, 4, Range::point(50.0).unwrap(), AttemptBilling::Settled),
    ];
    let window = |f: &QuotaForecast| f.windows.iter().find(|w| w.window_key == "five_hour").unwrap().clone();
    let now = c.observed_at;

    // Shared but no assumption about other clients: unknown, not a number.
    let f = forecast(&snap, &attempts, &HashMap::new(), now, fresh());
    assert!(matches!(window(&f).planning, Planning::Unknown { .. }));

    let assume = |reserve| {
        HashMap::from([(
            "five_hour".to_string(),
            WindowAssumptions { reserve, margin: 2.0, external: Range::new(0.0, 10.0) },
        )])
    };
    // No coverage proof: own usage is open from 0 to the sum of upper bounds.
    let f = forecast(&snap, &attempts, &assume(5.0), now, fresh());
    assert_eq!(window(&f).unreflected_own_cost, Range::new(0.0, 11.0));
    // A = 70 − 11 − 10 − 5 − 2 = 42 over 10 hours.
    let Planning::Period { available, per_hour, resets_at } = window(&f).planning else { panic!("period expected") };
    assert_eq!(available, 42.0);
    assert!(per_hour <= 4.2 && 4.2 - per_hour < 1e-12);
    assert_eq!(resets_at, Utc.timestamp_opt(reset, 0).unwrap());
    // A negative reserve cannot enlarge the budget.
    let f = forecast(&snap, &attempts, &assume(-50.0), now, fresh());
    assert!(matches!(window(&f).planning, Planning::Unknown { .. }));

    // The report covers this ledger through seq 1: only seq 2 (settled) and 3
    // (unknown, never refunded below 0) remain unreflected.
    snap.covered_through = Some(CoveredThrough { local_ledger_commit: CommitRef::new(ledger, LedgerSeq(1)) });
    let f = forecast(&snap, &attempts, &assume(5.0), now, fresh());
    assert_eq!(window(&f).unreflected_own_cost, Range::new(1.0, 8.0));

    // The same seq in a different ledger is not covered by that proof.
    let mut elsewhere = attempts.clone();
    elsewhere.push(attempt("sub", EntityId::new(), 1, Range::new(1.0, 1.5).unwrap(), AttemptBilling::Settled));
    let f = forecast(&snap, &elsewhere, &assume(5.0), now, fresh());
    assert_eq!(window(&f).unreflected_own_cost, Range::new(1.0, 9.5));

    // An attempt in another unit, or with an invalid cost, makes own usage unknown.
    let mut mixed = attempts.clone();
    mixed.push(LocalAttempt { unit: "tokens".into(), ..attempt("sub", ledger, 5, Range::point(5000.0).unwrap(), AttemptBilling::Settled) });
    let f = forecast(&snap, &mixed, &assume(5.0), now, fresh());
    assert_eq!(window(&f).unreflected_own_cost, None);
    assert!(matches!(window(&f).planning, Planning::Unknown { .. }));
    let mut bad = attempts.clone();
    bad.push(attempt("sub", ledger, 6, Range { lower: f64::NAN, upper: 1.0 }, AttemptBilling::Settled));
    assert_eq!(window(&forecast(&snap, &bad, &assume(5.0), now, fresh())).unreflected_own_cost, None);
}

#[test]
fn contradictory_overflowing_and_sensitive_observations_remain_unknown() {
    let c = ctx("sub", Attribution::Exclusive);
    let reset = (c.observed_at + Duration::hours(1)).timestamp();
    let mut snapshot = from_statusline(
        &c,
        &format!(r#"{{"rate_limits":{{"five_hour":{{"used_percentage":50,"resets_at":{reset}}}}}}}"#),
        true,
    );
    snapshot.windows[0].remaining = Some(Measurement::reported(120.0));
    assert_eq!(snapshot.windows[0].remaining_range(), None, "a remaining balance above capacity is contradictory");
    assert!(matches!(forecast(&snapshot, &[], &HashMap::new(), c.observed_at, fresh()).windows[0].planning,
        Planning::Unknown { .. }));
    snapshot.windows[0].remaining = Some(Measurement::reported(50.0));
    assert_eq!(snapshot.windows[0].remaining_range(), Range::point(50.0));
    snapshot.windows[0].used = Some(Measurement::reported(70.0));
    assert_eq!(snapshot.windows[0].remaining_range(), None, "remaining and used cannot jointly exceed capacity");

    let huge = Measurement { value: Some(f64::MAX), lower: None, upper: None,
        basis: Basis::ProviderReported, precision: Some(f64::MAX) };
    assert_eq!(huge.range(), None, "finite inputs may overflow when widened");
    let tiny_pad_at_max = Measurement { precision: Some(1.0), ..huge };
    assert_eq!(tiny_pad_at_max.range(), None, "rounding away a positive uncertainty is not a finite bound");
    let tiny_pad = Measurement { value: Some(1.0), precision: Some(f64::EPSILON / 4.0), ..huge };
    let tiny_range = tiny_pad.range().unwrap();
    assert!(tiny_range.lower < 1.0 && tiny_range.upper > 1.0);
    assert_eq!(Range::new(f64::NAN, 1.0), None);
    assert_eq!(Range::new(2.0, 1.0), None);
    assert_eq!(Range::point(f64::MAX).unwrap().checked_add(Range::point(f64::MAX).unwrap()), None);
    let inconsistent = Measurement { value: Some(f64::NAN), lower: Some(1.0), upper: Some(2.0),
        basis: Basis::ProviderReported, precision: None };
    assert_eq!(inconsistent.range(), None);

    let header = from_api_headers(&c, [("anthropic-ratelimit-tokens-limit", "Bearer synthetic-secret")]);
    assert_eq!(header.observation_status, ObservationStatus::Unavailable);
    assert_eq!(header.observation_error, Some(ObservationError::InvalidData));
    assert!(header.absent_windows.iter().any(|a| a.expected_window_key == "api:tokens" && a.reason == AbsentReason::Invalid));
    assert!(!serde_json::to_string(&header).unwrap().contains("synthetic-secret"));
    let invalid_reset = from_api_headers(&c, [("anthropic-ratelimit-tokens-reset", "bad")]);
    assert_eq!(invalid_reset.observation_status, ObservationStatus::Unavailable);
    assert!(invalid_reset.absent_windows.iter().any(|a| a.expected_window_key == "api:tokens" && a.reason == AbsentReason::Invalid));
    let unsupported = from_api_headers(&c, []);
    assert_eq!(unsupported.observation_status, ObservationStatus::Unsupported);
    let failure = observation_failed(&c, SourceKind::CliUi, &STATUSLINE_WINDOWS, ObservationError::RateLimited);
    assert_eq!(failure.observation_error, Some(ObservationError::RateLimited));

    let exact = 9_007_199_254_740_995_u64;
    let large = from_api_headers(&c, [("anthropic-ratelimit-requests-remaining", "9007199254740995")]);
    let interval = large.window("api:requests").unwrap().remaining_range().unwrap();
    assert!((interval.lower as u128) <= u128::from(exact));
    assert!((interval.upper as u128) >= u128::from(exact));
    assert!(interval.lower < interval.upper, "inexact f64 must remain a conservative range");
    let contradictory = from_api_headers(&c, [
        ("anthropic-ratelimit-requests-limit", "9007199254740992"),
        ("anthropic-ratelimit-requests-remaining", "9007199254740993"),
    ]);
    assert!(contradictory.window("api:requests").is_none());
    assert!(contradictory.absent_windows.iter().any(|a| a.expected_window_key == "api:requests" && a.reason == AbsentReason::Invalid));
    let rounded = from_api_headers(&c, [
        ("anthropic-ratelimit-tokens-limit", "800"),
        ("anthropic-ratelimit-tokens-remaining", "1000"),
    ]);
    assert_eq!(rounded.window("api:tokens").unwrap().remaining_range(), Range::new(500.0, 800.0),
        "rounding can produce a reported count above the exact limit");

    snapshot.windows[0].remaining = None;
    snapshot.windows[0].used = Some(Measurement::reported(50.0));
    let ledger = EntityId::new();
    let attempt = |seq| LocalAttempt {
        request_id: EntityId::new(), provider_id: c.provider_id.clone(), account_scope_ref: c.account_scope_ref.clone(),
        window_key: "five_hour".into(), unit: "percent".into(),
        ledger_commit: CommitRef::new(ledger, LedgerSeq(seq)), cost: Range::point(f64::MAX).unwrap(),
        billing: AttemptBilling::Settled,
    };
    let overflow = forecast(&snapshot, &[attempt(1), attempt(2)], &HashMap::new(), c.observed_at, fresh());
    assert_eq!(overflow.windows[0].unreflected_own_cost, None);
    assert!(matches!(overflow.windows[0].planning, Planning::Unknown { .. }));

    snapshot.windows[0].capacity = None;
    snapshot.windows[0].used = None;
    snapshot.windows[0].remaining = Some(Measurement::reported(f64::MAX));
    snapshot.windows[0].resets_at = Some(c.observed_at + Duration::seconds(1));
    assert!(matches!(forecast(&snapshot, &[], &HashMap::new(), c.observed_at, fresh()).windows[0].planning,
        Planning::Unknown { .. }), "an infinite hourly rate is not a usable forecast");
}

#[test]
fn shared_token_bucket_needs_an_external_usage_bound() {
    let c = ctx("shared-key", Attribution::SharedUnobserved);
    let snapshot = from_api_headers(&c, [
        ("anthropic-ratelimit-requests-limit", "100"),
        ("anthropic-ratelimit-requests-remaining", "10"),
    ]);
    let absent = forecast(&snapshot, &[], &HashMap::new(), c.observed_at, fresh());
    assert!(matches!(absent.windows[0].planning, Planning::Unknown { .. }));
    let assumptions = HashMap::from([("api:requests".into(), WindowAssumptions {
        reserve: 0.0, margin: 0.0, external: Range::new(1.0, 3.0),
    })]);
    let bounded = forecast(&snapshot, &[], &assumptions, c.observed_at, fresh());
    assert!(matches!(bounded.windows[0].planning, Planning::Replenishing { remaining, .. }
        if Some(remaining) == Range::new(7.0, 9.0)));
    let reserved = HashMap::from([("api:requests".into(), WindowAssumptions {
        reserve: 1.0, margin: 0.5, external: Range::new(1.0, 3.0),
    })]);
    let bounded = forecast(&snapshot, &[], &reserved, c.observed_at, fresh());
    assert!(matches!(bounded.windows[0].planning, Planning::Replenishing { remaining, .. }
        if Some(remaining) == Range::new(5.5, 7.5)));
}

#[test]
fn floating_point_costs_cannot_raise_the_conservative_budget() {
    let c = ctx("sub", Attribution::Exclusive);
    let reset = (c.observed_at + Duration::hours(1)).timestamp();
    let mut snapshot = from_statusline(&c,
        &format!(r#"{{"rate_limits":{{"five_hour":{{"used_percentage":50,"resets_at":{reset}}}}}}}"#), true);
    snapshot.windows[0].capacity = None;
    snapshot.windows[0].used = None;
    snapshot.windows[0].remaining = Some(Measurement::reported(1.0));
    let attempt = LocalAttempt {
        request_id: EntityId::new(), provider_id: c.provider_id.clone(), account_scope_ref: c.account_scope_ref.clone(),
        window_key: "five_hour".into(), unit: "percent".into(),
        ledger_commit: CommitRef::new(EntityId::new(), LedgerSeq(1)),
        cost: Range::point(f64::EPSILON / 4.0).unwrap(), billing: AttemptBilling::Settled,
    };
    let result = forecast(&snapshot, &[attempt], &HashMap::new(), c.observed_at, fresh());
    let Planning::Period { available, .. } = result.windows[0].planning else { panic!("finite period expected") };
    assert!(available < 1.0, "a positive unreflected charge cannot disappear by rounding");
    let sum = Range::point(1.0).unwrap().checked_add(Range::point(f64::EPSILON / 4.0).unwrap()).unwrap();
    assert!(sum.upper > 1.0, "the upper cost bound must contain the exact sum");
    snapshot.windows[0].resets_at = Some(c.observed_at + Duration::milliseconds(1500));
    let result = forecast(&snapshot, &[], &HashMap::new(), c.observed_at, fresh());
    let Planning::Period { per_hour, .. } = result.windows[0].planning else { panic!("subsecond period expected") };
    assert!(per_hour <= 2400.0 && per_hour > 2399.999, "fractional seconds must not shorten the period");
}
