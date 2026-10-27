//! QA05/QA06 decision and admission behavior on constructed forecasts.
use std::collections::HashMap;

use chrono::{DateTime, Duration, TimeZone, Utc};
use shenko_quota::*;
use shenko_types::ids::EntityId;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 27, 12, 0, 0).unwrap()
}

fn wref(key: &str) -> WindowRef {
    WindowRef { provider_id: "anthropic".into(), account_scope_ref: "sub".into(), window_key: key.into() }
}

/// A forecast for account "sub" with (window, planning, remaining) rows.
fn forecast_of(rows: Vec<(&str, Planning, Option<Range>)>) -> QuotaForecast {
    QuotaForecast {
        forecast_id: EntityId::new(),
        based_on_snapshot: EntityId::new(),
        provider_id: "anthropic".into(),
        account_scope_ref: "sub".into(),
        computed_at: now(),
        freshness: Freshness::Fresh,
        windows: rows
            .into_iter()
            .map(|(key, planning, remaining)| WindowForecast {
                window_key: key.into(),
                unit: "percent".into(),
                remaining,
                unreflected_own_cost: Some(Range::zero()),
                external_cost: Some(Range::zero()),
                basis: Freshness::Fresh,
                planning,
            })
            .collect(),
        unobserved: Vec::new(),
    }
}

fn period(available: f64, per_hour: f64, hours: i64) -> Planning {
    Planning::Period { available, per_hour, resets_at: now() + Duration::hours(hours) }
}

fn profile(id: &str, five: (f64, f64), week: (f64, f64)) -> ModelProfile {
    ModelProfile {
        profile_id: id.into(),
        model_ref: format!("model/{id}"),
        local: false,
        cost_per_request: HashMap::from([
            (wref("five_hour"), Range::new(five.0, five.1)),
            (wref("seven_day"), Range::new(week.0, week.1)),
        ]),
    }
}

fn policy() -> QuotaPolicy {
    QuotaPolicy {
        policy_id: EntityId::new(),
        revision: 1,
        profiles: vec![
            profile("smart", (1.0, 2.0), (0.2, 0.4)),
            profile("cheap", (0.2, 0.4), (0.05, 0.1)),
            ModelProfile { profile_id: "local".into(), model_ref: "model/local".into(), local: true, cost_per_request: HashMap::new() },
        ],
        assumptions: HashMap::new(),
        min_progress_per_hour: 2.0,
        switching_hysteresis: 0.2,
        on_unknown: UnknownQuota::Block,
        allow_overage: false,
        spending_authority_ref: None,
        decision_ttl: Duration::minutes(20),
    }
}

fn cloud(d: &PolicyDecision) -> (&str, f64) {
    match &d.choice {
        Choice::Cloud { profile_id, rate_per_hour, .. } => (profile_id.as_str(), *rate_per_hour),
        other => panic!("expected a cloud choice, got {other:?}"),
    }
}

/// QA05: every overlapping window bounds the rate, the weekly window can
/// rule out the smart profile even when the five-hour window has room,
/// hysteresis avoids flapping, and a too-slow cloud yields to local.
#[test]
fn decision_respects_every_window_and_falls_back_to_local() {
    let p = policy();
    // 5h: 10%/h sustainable; week: 0.5%/h. smart = min(10/2, 0.5/0.4) = 1.25 < 2.
    let tight_week = forecast_of(vec![
        ("five_hour", period(20.0, 10.0, 2), Some(Range::point(60.0))),
        ("seven_day", period(40.0, 0.5, 80), Some(Range::point(50.0))),
    ]);
    let d = decide(&p, &[tight_week.clone()], true, None, now());
    assert_eq!(cloud(&d), ("cheap", 5.0), "cheap = min(10/0.4, 0.5/0.1)");
    assert!(d.considered.contains(&("smart".to_string(), Some(1.25))));

    let roomy = forecast_of(vec![
        ("five_hour", period(20.0, 10.0, 2), Some(Range::point(60.0))),
        ("seven_day", period(400.0, 5.0, 80), Some(Range::point(90.0))),
    ]);
    assert_eq!(cloud(&decide(&p, &[roomy], true, None, now())).0, "smart");

    // smart now sustains 2.1/h: preferred, but not by the 20% margin over
    // the 2/h minimum, so the previous cheap choice is kept.
    let marginal = forecast_of(vec![
        ("five_hour", period(20.0, 4.2, 2), Some(Range::point(60.0))),
        ("seven_day", period(400.0, 5.0, 80), Some(Range::point(90.0))),
    ]);
    let kept = decide(&p, &[marginal], true, Some(&d), now());
    assert_eq!(cloud(&kept).0, "cheap");

    // Both cloud profiles below the minimum progress rate.
    let starved = forecast_of(vec![
        ("five_hour", period(1.0, 0.5, 2), Some(Range::point(5.0))),
        ("seven_day", period(400.0, 5.0, 80), Some(Range::point(90.0))),
    ]);
    let d = decide(&p, &[starved.clone()], true, None, now());
    assert_eq!(d.choice, Choice::Local { profile_id: "local".into() });
    // Without a local backend, the faster of the slow cloud profiles is paced.
    assert_eq!(cloud(&decide(&p, &[starved], false, None, now())), ("cheap", 1.25));
}

/// QA02/QA05: admissions are paced to the decided rate as quota_policy_wait,
/// a reserve-limited zero budget is a wait (not exhaustion), only a provider
/// at its limit is exhausted, and unknown quota follows the policy.
#[test]
fn admission_paces_and_keeps_wait_exhaustion_and_unknown_apart() {
    let p = policy();
    let f = forecast_of(vec![
        ("five_hour", period(20.0, 10.0, 2), Some(Range::point(60.0))),
        ("seven_day", period(40.0, 0.5, 80), Some(Range::point(50.0))),
    ]);
    let d = decide(&p, &[f], false, None, now());
    let mut pacer = Pacer::default();
    let state = |o: AdmissionOutcome| match o {
        AdmissionOutcome::Quota(a) => a,
        other => panic!("expected a quota admission, got {other:?}"),
    };
    let first = state(admit(&p, &d, EntityId::new(), &mut pacer, now()));
    assert_eq!(first.state, AdmissionState::Admitted);
    assert_eq!(first.estimated_cost.len(), 2, "reserves against both windows");
    // cheap at 5/h: the next admission is 12 minutes later.
    let early = state(admit(&p, &d, EntityId::new(), &mut pacer, now() + Duration::minutes(5)));
    assert_eq!(early.state, AdmissionState::QuotaPolicyWait);
    assert_eq!(early.next_eligible_at, Some(now() + Duration::minutes(12)));
    let on_time = state(admit(&p, &d, EntityId::new(), &mut pacer, now() + Duration::minutes(12)));
    assert_eq!(on_time.state, AdmissionState::Admitted);
    // A decision past its validity admits nothing until recomputed.
    let late = state(admit(&p, &d, EntityId::new(), &mut pacer, now() + Duration::minutes(30)));
    assert_eq!(late.state, AdmissionState::Eligible);

    // Budget is zero only because of reserve/margin: wait for the reset.
    let reserved = forecast_of(vec![
        ("five_hour", period(0.0, 0.0, 2), Some(Range::point(3.0))),
        ("seven_day", period(40.0, 0.5, 80), Some(Range::point(50.0))),
    ]);
    let d = decide(&p, &[reserved], false, None, now());
    assert_eq!(d.choice, Choice::Wait { until: Some(now() + Duration::hours(2)) });
    assert_eq!(state(admit(&p, &d, EntityId::new(), &mut Pacer::default(), now())).state, AdmissionState::QuotaPolicyWait);

    // The provider reports nothing remaining: exhausted, with the windows.
    let at_limit = forecast_of(vec![
        ("five_hour", period(0.0, 0.0, 2), Some(Range::point(0.0))),
        ("seven_day", period(40.0, 0.5, 80), Some(Range::point(50.0))),
    ]);
    let d = decide(&p, &[at_limit], false, None, now());
    assert!(matches!(&d.choice, Choice::ProviderExhausted { windows, .. } if windows == &vec![wref("five_hour")]));
    assert_eq!(state(admit(&p, &d, EntityId::new(), &mut Pacer::default(), now())).state, AdmissionState::ProviderExhausted);

    // The weekly window has no usable reading.
    let unseen = forecast_of(vec![
        ("five_hour", period(20.0, 10.0, 2), Some(Range::point(60.0))),
        ("seven_day", Planning::Unknown { reason: "stale".into() }, None),
    ]);
    let d = decide(&p, &[unseen.clone()], false, None, now());
    assert!(matches!(d.choice, Choice::ObservationBlocked { .. }));
    assert_eq!(state(admit(&p, &d, EntityId::new(), &mut Pacer::default(), now())).state, AdmissionState::ObservationBlocked);
    let lenient = QuotaPolicy { on_unknown: UnknownQuota::Admit, ..policy() };
    let d = decide(&lenient, &[unseen], false, None, now());
    assert!(matches!(&d.choice, Choice::Cloud { unbounded_windows, .. } if unbounded_windows == &vec![wref("seven_day")]));
}

/// QA06: an exhausted cloud turns to an available local backend, no backend
/// at all is a capacity block rather than a quota state, overage is never
/// switched on by the optimizer, and cancelling after send keeps the cost.
#[test]
fn exhaustion_fallback_capacity_block_and_cancellation() {
    let p = policy();
    let at_limit = forecast_of(vec![
        ("five_hour", period(0.0, 0.0, 2), Some(Range::point(0.0))),
        ("seven_day", period(0.0, 0.0, 80), Some(Range::point(0.0))),
    ]);
    let d = decide(&p, &[at_limit.clone()], true, None, now());
    assert_eq!(d.choice, Choice::Local { profile_id: "local".into() });
    let AdmissionOutcome::Quota(a) = admit(&p, &d, EntityId::new(), &mut Pacer::default(), now()) else {
        panic!("local admission is a quota record");
    };
    assert!(a.state == AdmissionState::Admitted && a.estimated_cost.is_empty());
    assert!(!p.allow_overage, "the optimizer never enables extra paid usage");

    let none = QuotaPolicy { profiles: Vec::new(), ..policy() };
    let d = decide(&none, &[at_limit], false, None, now());
    assert_eq!(d.choice, Choice::NoBackend);
    assert!(matches!(admit(&none, &d, EntityId::new(), &mut Pacer::default(), now()), AdmissionOutcome::CapacityBlocked { .. }));

    let roomy = forecast_of(vec![
        ("five_hour", period(20.0, 10.0, 2), Some(Range::point(60.0))),
        ("seven_day", period(400.0, 5.0, 80), Some(Range::point(90.0))),
    ]);
    let d = decide(&p, &[roomy], false, None, now());
    let AdmissionOutcome::Quota(admitted) = admit(&p, &d, EntityId::new(), &mut Pacer::default(), now()) else {
        panic!("cloud admission is a quota record");
    };
    let (unsent, released) = cancel(&admitted, false);
    assert!(unsent.state == AdmissionState::Cancelled && released.is_empty());
    let (sent, retained) = cancel(&admitted, true);
    assert_eq!(sent.record_revision, 1);
    assert!(retained.contains(&(wref("five_hour"), Range::new(0.0, 2.0))), "stopping a sent call does not make it free");
}
