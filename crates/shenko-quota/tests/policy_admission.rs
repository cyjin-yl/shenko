//! QA05/QA06 decision and admission behavior on constructed forecasts.
use std::collections::HashMap;

use chrono::{DateTime, Duration, TimeZone, Utc};
use shenko_quota::*;
use shenko_types::{commit::CommitRef, ids::{EntityId, LedgerSeq}};

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
        source_validity: SourceValidity::FreshUntil { exclusive: now() + Duration::hours(24) },
        windows: rows
            .into_iter()
            .map(|(key, planning, remaining)| WindowForecast {
                window_key: key.into(),
                unit: "percent".into(),
                remaining,
                unreflected_own_cost: Some(Range::zero()),
                external_cost: Some(Range::zero()),
                assumptions: WindowAssumptions::default(),
                accounted_attempts: Vec::new(),
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
        cost_per_request: vec![
            WindowCost { window: wref("five_hour"), unit: "percent".into(), amount: Range::new(five.0, five.1).unwrap() },
            WindowCost { window: wref("seven_day"), unit: "percent".into(), amount: Range::new(week.0, week.1).unwrap() },
        ],
    }
}

fn policy() -> QuotaPolicy {
    QuotaPolicy {
        policy_id: EntityId::new(),
        revision: 1,
        profiles: vec![
            profile("smart", (1.0, 2.0), (0.2, 0.4)),
            profile("cheap", (0.2, 0.4), (0.05, 0.1)),
            ModelProfile { profile_id: "local".into(), model_ref: "model/local".into(), local: true, cost_per_request: Vec::new() },
        ],
        assumptions: Vec::new(),
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
        Choice::Cloud { profile_id, rate_per_hour: Some(rate), .. } => (profile_id.as_str(), *rate),
        other => panic!("expected a cloud choice, got {other:?}"),
    }
}

fn availability(policy: &QuotaPolicy, local: bool) -> HashMap<String, BackendState> {
    policy.profiles.iter().map(|profile| (profile.profile_id.clone(),
        if profile.local && !local { BackendState::Unavailable } else { BackendState::Available })).collect()
}

fn decide_fixture(policy: &QuotaPolicy, forecasts: &[QuotaForecast], local: bool,
    previous: Option<&PolicyDecision>, at: DateTime<Utc>) -> PolicyDecision {
    decide(policy, forecasts, &availability(policy, local), previous, at).unwrap()
}

fn single_window_policy(key: &str, unit: &str, costs: &[(&str, f64)]) -> QuotaPolicy {
    let mut p = policy();
    p.min_progress_per_hour = 0.0;
    p.profiles = costs.iter().map(|(id, cost)| ModelProfile {
        profile_id: (*id).into(), model_ref: format!("model/{id}"), local: false,
        cost_per_request: vec![WindowCost { window: wref(key), unit: unit.into(), amount: Range::point(*cost).unwrap() }],
    }).collect();
    p
}

fn admit_fixture(policy: &QuotaPolicy, decision: &PolicyDecision, request_id: EntityId,
    pacer: &mut Pacer, at: DateTime<Utc>) -> AdmissionOutcome {
    let available = decision.availability.iter().map(|item| (item.profile_id.clone(), item.state)).collect();
    admit(policy, decision, request_id, pacer, &available, at)
}

/// QA05: every overlapping window bounds the rate, the weekly window can
/// rule out the smart profile even when the five-hour window has room,
/// hysteresis avoids flapping, and a too-slow cloud yields to local.
#[test]
fn decision_respects_every_window_and_falls_back_to_local() {
    let p = policy();
    // 5h: 10%/h sustainable; week: 0.5%/h. smart = min(10/2, 0.5/0.4) = 1.25 < 2.
    let tight_week = forecast_of(vec![
        ("five_hour", period(20.0, 10.0, 2), Range::point(60.0)),
        ("seven_day", period(40.0, 0.5, 80), Range::point(50.0)),
    ]);
    let d = decide_fixture(&p, &[tight_week.clone()], true, None, now());
    let (id, rate) = cloud(&d);
    assert_eq!(id, "cheap");
    assert!(rate <= 5.0 && 5.0 - rate < 1e-12);
    assert!(d.considered.iter().any(|(id, rate)| id == "smart" && rate.is_some_and(|rate| rate <= 1.25 && 1.25 - rate < 1e-12)));

    let roomy = forecast_of(vec![
        ("five_hour", period(20.0, 10.0, 2), Range::point(60.0)),
        ("seven_day", period(80.0, 5.0, 16), Range::point(90.0)),
    ]);
    assert_eq!(cloud(&decide_fixture(&p, &[roomy], true, None, now())).0, "smart");

    // smart now sustains 2.1/h: preferred, but not by the 20% margin over
    // the 2/h minimum, so the previous cheap choice is kept.
    let marginal = forecast_of(vec![
        ("five_hour", period(20.0, 4.2, 2), Range::point(60.0)),
        ("seven_day", period(80.0, 5.0, 16), Range::point(90.0)),
    ]);
    let kept = decide_fixture(&p, &[marginal], true, Some(&d), now());
    assert_eq!(cloud(&kept).0, "cheap");

    // Both cloud profiles below the minimum progress rate.
    let starved = forecast_of(vec![
        ("five_hour", period(1.0, 0.5, 2), Range::point(5.0)),
        ("seven_day", period(80.0, 5.0, 16), Range::point(90.0)),
    ]);
    let d = decide_fixture(&p, &[starved.clone()], true, None, now());
    assert_eq!(d.choice, Choice::Local { profile_id: "local".into() });
    // Without a local backend, the faster of the slow cloud profiles is paced.
    let slow = decide_fixture(&p, &[starved], false, None, now());
    let (id, rate) = cloud(&slow);
    assert_eq!(id, "cheap");
    assert!(rate <= 1.25 && 1.25 - rate < 1e-12);
}

/// QA02/QA05: admissions are paced to the decided rate as quota_policy_wait,
/// a reserve-limited zero budget is a wait (not exhaustion), only a provider
/// at its limit is exhausted, and unknown quota follows the policy.
#[test]
fn admission_paces_and_keeps_wait_exhaustion_and_unknown_apart() {
    let p = policy();
    let f = forecast_of(vec![
        ("five_hour", period(20.0, 10.0, 2), Range::point(60.0)),
        ("seven_day", period(40.0, 0.5, 80), Range::point(50.0)),
    ]);
    let d = decide_fixture(&p, &[f], false, None, now());
    let mut pacer = Pacer::default();
    let state = |o: AdmissionOutcome| match o {
        AdmissionOutcome::Quota(a) => a,
        other => panic!("expected a quota admission, got {other:?}"),
    };
    let first = state(admit_fixture(&p, &d, EntityId::new(), &mut pacer, now()));
    assert_eq!(first.state, AdmissionState::Admitted);
    assert!(first.estimated_cost.iter().any(|cost| cost.window == wref("five_hour")
        && cost.unit == "percent" && Some(cost.amount) == Range::new(0.2, 0.4)));
    assert!(first.estimated_cost.iter().any(|cost| cost.window == wref("seven_day")
        && cost.unit == "percent" && Some(cost.amount) == Range::new(0.05, 0.1)));
    // cheap at 5/h: the next admission is 12 minutes later.
    let next_request = EntityId::new();
    let early = state(admit_fixture(&p, &d, next_request, &mut pacer, now() + Duration::minutes(5)));
    assert_eq!(early.state, AdmissionState::QuotaPolicyWait);
    let next = early.next_eligible_at.unwrap();
    assert!(next >= now() + Duration::minutes(12) && next <= now() + Duration::minutes(12) + Duration::milliseconds(2));
    let on_time = state(admit_fixture(&p, &d, next_request, &mut pacer, next));
    assert_eq!(on_time.state, AdmissionState::Admitted);
    assert_eq!(on_time.admission_id, early.admission_id);
    assert_eq!(on_time.record_revision, early.record_revision + 1);
    let mut rollback = Pacer::default();
    let unsent = state(admit_fixture(&p, &d, EntityId::new(), &mut rollback, now()));
    cancel(&mut rollback, &unsent, DispatchEvidence::NeverDispatched).unwrap();
    assert_eq!(state(admit_fixture(&p, &d, EntityId::new(), &mut rollback, now())).state, AdmissionState::Admitted,
        "a proven unsent request must release its pacing slot");
    // A decision past its validity admits nothing until recomputed.
    let late = state(admit_fixture(&p, &d, EntityId::new(), &mut pacer, now() + Duration::minutes(30)));
    assert_eq!(late.state, AdmissionState::Eligible);

    // Budget is zero only because of reserve/margin: wait for the reset.
    let reserved = forecast_of(vec![
        ("five_hour", period(0.0, 0.0, 2), Range::point(3.0)),
        ("seven_day", period(40.0, 0.5, 80), Range::point(50.0)),
    ]);
    let d = decide_fixture(&p, &[reserved], false, None, now());
    assert_eq!(d.choice, Choice::Wait { until: Some(now() + Duration::hours(2)) });
    assert_eq!(state(admit_fixture(&p, &d, EntityId::new(), &mut Pacer::default(), now())).state, AdmissionState::QuotaPolicyWait);

    // The provider reports nothing remaining: exhausted, with the windows.
    let at_limit = forecast_of(vec![
        ("five_hour", period(0.0, 0.0, 2), Range::point(0.0)),
        ("seven_day", period(40.0, 0.5, 80), Range::point(50.0)),
    ]);
    let d = decide_fixture(&p, &[at_limit], false, None, now());
    assert!(matches!(&d.choice, Choice::ProviderExhausted { windows, .. } if windows == &vec![wref("five_hour")]));
    assert_eq!(state(admit_fixture(&p, &d, EntityId::new(), &mut Pacer::default(), now())).state, AdmissionState::ProviderExhausted);

    // The weekly window has no usable reading.
    let unseen = forecast_of(vec![
        ("five_hour", period(20.0, 10.0, 2), Range::point(60.0)),
        ("seven_day", Planning::Unknown { reason: "stale".into() }, None),
    ]);
    let d = decide_fixture(&p, &[unseen.clone()], false, None, now());
    assert!(matches!(d.choice, Choice::ObservationBlocked { .. }));
    assert_eq!(state(admit_fixture(&p, &d, EntityId::new(), &mut Pacer::default(), now())).state, AdmissionState::ObservationBlocked);
    let lenient = QuotaPolicy { on_unknown: UnknownQuota::Admit, ..policy() };
    let d = decide_fixture(&lenient, &[unseen], false, None, now());
    assert!(matches!(&d.choice, Choice::Cloud { unbounded_windows, .. } if unbounded_windows == &vec![wref("seven_day")]));
}

/// QA06: an exhausted cloud turns to an available local backend, no backend
/// at all is a capacity block rather than a quota state, overage is never
/// switched on by the optimizer, and cancelling after send keeps the cost.
#[test]
fn exhaustion_fallback_capacity_block_and_cancellation() {
    let p = policy();
    let at_limit = forecast_of(vec![
        ("five_hour", period(0.0, 0.0, 2), Range::point(0.0)),
        ("seven_day", period(0.0, 0.0, 80), Range::point(0.0)),
    ]);
    let d = decide_fixture(&p, &[at_limit.clone()], true, None, now());
    assert_eq!(d.choice, Choice::Local { profile_id: "local".into() });
    let AdmissionOutcome::Quota(a) = admit_fixture(&p, &d, EntityId::new(), &mut Pacer::default(), now()) else {
        panic!("local admission is a quota record");
    };
    assert!(a.state == AdmissionState::Admitted && a.estimated_cost.is_empty());

    let none = QuotaPolicy { profiles: Vec::new(), ..policy() };
    let d = decide_fixture(&none, &[at_limit], false, None, now());
    assert_eq!(d.choice, Choice::NoBackend);
    assert!(matches!(admit_fixture(&none, &d, EntityId::new(), &mut Pacer::default(), now()), AdmissionOutcome::CapacityBlocked { .. }));

    let roomy = forecast_of(vec![
        ("five_hour", period(20.0, 10.0, 2), Range::point(60.0)),
        ("seven_day", period(80.0, 5.0, 16), Range::point(90.0)),
    ]);
    let d = decide_fixture(&p, &[roomy], false, None, now());
    let mut sent_pacer = Pacer::default();
    let AdmissionOutcome::Quota(admitted) = admit_fixture(&p, &d, EntityId::new(), &mut sent_pacer, now()) else {
        panic!("cloud admission is a quota record");
    };
    let mut unsent_pacer = Pacer::default();
    let AdmissionOutcome::Quota(other) = admit_fixture(&p, &d, EntityId::new(), &mut unsent_pacer, now()) else {
        panic!("cloud admission is a quota record");
    };
    let (unsent, released) = cancel(&mut unsent_pacer, &other, DispatchEvidence::NeverDispatched).unwrap();
    assert!(unsent.state == AdmissionState::Cancelled && released.is_empty());
    let commit = CommitRef::new(EntityId::new(), LedgerSeq(1));
    let attempts: Vec<_> = admitted.estimated_cost.iter().map(|cost| LocalAttempt {
        request_id: admitted.request_id, provider_id: cost.window.provider_id.clone(),
        account_scope_ref: cost.window.account_scope_ref.clone(), window_key: cost.window.window_key.clone(),
        unit: cost.unit.clone(), ledger_commit: commit, cost: cost.amount, billing: AttemptBilling::Unknown,
    }).collect();
    let armed = mark_armed(&mut sent_pacer, &admitted, &attempts).unwrap();
    assert_eq!(mark_armed(&mut sent_pacer, &admitted, &attempts).unwrap(), armed);
    let (sent, retained) = cancel(&mut sent_pacer, &armed, DispatchEvidence::ArmedOrUnknown).unwrap();
    assert_eq!(sent.record_revision, 2);
    assert!(retained.iter().any(|cost| cost.window == wref("five_hour")
        && cost.unit == "percent" && Some(cost.amount) == Range::new(0.0, 2.0)), "stopping a sent call does not make it free");
}

#[test]
fn policy_rejects_unbound_costs_conflicts_and_invalid_rates() {
    let mut p = policy();
    p.profiles.truncate(1);
    let available = availability(&p, false);
    let mut empty = p.clone();
    empty.profiles[0].cost_per_request.clear();
    assert_eq!(decide(&empty, &[], &available, None, now()), Err(PolicyError::MissingCloudCost));
    let mut overage = p.clone();
    overage.allow_overage = true;
    assert_eq!(decide(&overage, &[], &available, None, now()), Err(PolicyError::UnsupportedOverage));

    let full = forecast_of(vec![
        ("five_hour", period(20.0, 10.0, 2), Range::point(60.0)),
        ("seven_day", period(40.0, 0.5, 80), Range::point(50.0)),
    ]);
    let mut wrong_unit = full.clone();
    wrong_unit.windows[0].unit = "tokens".into();
    let mismatch = decide(&p, &[wrong_unit], &available, None, now()).unwrap();
    assert!(matches!(mismatch.choice, Choice::ObservationBlocked { .. }));
    let mut exhausted = full.clone();
    exhausted.forecast_id = EntityId::new();
    exhausted.windows[0].planning = period(0.0, 0.0, 2);
    exhausted.windows[0].remaining = Range::point(0.0);
    let a = decide(&p, &[full.clone(), exhausted.clone()], &available, None, now()).unwrap();
    let b = decide(&p, &[exhausted, full.clone()], &available, None, now()).unwrap();
    assert!(matches!(a.choice, Choice::ObservationBlocked { .. }));
    assert_eq!(a.choice, b.choice, "conflicting sources cannot be selected by input order");

    let mut invalid_rate = full.clone();
    if let Planning::Period { per_hour, .. } = &mut invalid_rate.windows[0].planning { *per_hour = f64::NAN; }
    assert!(matches!(decide(&p, &[invalid_rate], &available, None, now()).unwrap().choice,
        Choice::ObservationBlocked { .. }));
    let decision = decide(&p, &[full.clone()], &available, None, now()).unwrap();
    let mut tampered = decision.clone();
    if let Choice::Cloud { rate_per_hour, .. } = &mut tampered.choice { *rate_per_hour = Some(f64::NAN); }
    assert!(matches!(admit(&p, &tampered, EntityId::new(), &mut Pacer::default(), &available, now()),
        AdmissionOutcome::Invalid { .. }));
    let mut changed = p.clone();
    changed.profiles[0].cost_per_request[0].amount = Range::point(3.0).unwrap();
    assert!(matches!(admit(&changed, &decision, EntityId::new(), &mut Pacer::default(), &available, now()),
        AdmissionOutcome::Invalid { .. }), "reusing a revision cannot silently replace policy contents");
    let mut tampered_backend = decision.clone();
    tampered_backend.availability[0].state = BackendState::Unknown;
    assert!(matches!(admit(&p, &tampered_backend, EntityId::new(), &mut Pacer::default(), &available, now()),
        AdmissionOutcome::Invalid { .. }));
    assert!(matches!(decide(&p, &[full.clone()], &HashMap::new(), None, now()).unwrap().choice,
        Choice::CapacityUnknown { .. }));
    let unavailable = HashMap::from([("smart".into(), BackendState::Unavailable)]);
    assert_eq!(decide(&p, &[full.clone()], &unavailable, None, now()).unwrap().choice, Choice::NoBackend);
    assert!(matches!(admit(&p, &decision, EntityId::new(), &mut Pacer::default(), &unavailable, now()),
        AdmissionOutcome::CapacityBlocked { .. }));
    assert!(matches!(admit(&p, &decision, EntityId::new(), &mut Pacer::default(), &HashMap::new(), now()),
        AdmissionOutcome::CapacityUnknown { .. }));

    let json = serde_json::to_string(&p).unwrap();
    let reopened: QuotaPolicy = serde_json::from_str(&json).unwrap();
    let replayed = decide(&reopened, &[full], &available, None, now()).unwrap();
    assert_eq!(replayed.choice, decision.choice);
    assert_eq!(replayed.policy_digest, decision.policy_digest);
}

#[test]
fn bucket_reservations_survive_replay_cancel_and_coverage() {
    let window = wref("api:requests");
    let mut p = policy();
    p.profiles = vec![ModelProfile { profile_id: "cloud".into(), model_ref: "model/cloud".into(), local: false,
        cost_per_request: vec![WindowCost { window: window.clone(), unit: "requests".into(), amount: Range::point(1.0).unwrap() }] }];
    let available = availability(&p, false);
    let ctx = ObservationContext { provider_id: "anthropic".into(), account_scope_ref: "sub".into(),
        source_version: None, attribution: Attribution::Exclusive, observed_at: now() };
    let mut snapshot = from_api_headers(&ctx, [
        ("anthropic-ratelimit-requests-limit", "1"),
        ("anthropic-ratelimit-requests-remaining", "1"),
    ]);
    let options = ForecastOptions { ttl: Duration::minutes(5), stale: StaleUse::Refuse };
    let first_forecast = forecast(&snapshot, &[], &HashMap::new(), now(), options);
    let decision = decide(&p, &[first_forecast], &available, None, now()).unwrap();
    assert!(matches!(decision.choice, Choice::Cloud { rate_per_hour: None, .. }));
    let encoded = serde_json::to_string(&decision).unwrap();
    let decision: PolicyDecision = serde_json::from_str(&encoded).unwrap();
    let first_id = EntityId::new();
    let second_id = EntityId::new();
    let mut pacer = Pacer::default();
    let AdmissionOutcome::Quota(first) = admit(&p, &decision, first_id, &mut pacer, &available, now()) else { panic!("first must admit") };
    assert_eq!(first.state, AdmissionState::Admitted);
    assert!(matches!(admit(&p, &decision, first_id, &mut pacer, &available, now()),
        AdmissionOutcome::Existing { admission } if admission.admission_id == first.admission_id));
    let AdmissionOutcome::Quota(waiting) = admit(&p, &decision, second_id, &mut pacer, &available, now()) else { panic!("second must wait") };
    assert_eq!(waiting.state, AdmissionState::QuotaPolicyWait);
    let saved = serde_json::to_string(&pacer).unwrap();
    let mut reopened: Pacer = serde_json::from_str(&saved).unwrap();
    let (cancelled, released) = cancel(&mut reopened, &first, DispatchEvidence::NeverDispatched).unwrap();
    assert_eq!(cancelled.state, AdmissionState::Cancelled);
    assert!(released.is_empty());
    let AdmissionOutcome::Quota(second) = admit(&p, &decision, second_id, &mut reopened, &available, now()) else { panic!("released budget must admit") };
    assert_eq!(second.state, AdmissionState::Admitted);
    assert_eq!(second.admission_id, waiting.admission_id);
    assert_eq!(second.record_revision, waiting.record_revision + 1);

    let mut sent_pacer = Pacer::default();
    let sent_id = EntityId::new();
    let AdmissionOutcome::Quota(sent) = admit(&p, &decision, sent_id, &mut sent_pacer, &available, now()) else { panic!("first must admit") };
    assert_eq!(cancel(&mut sent_pacer, &sent, DispatchEvidence::ArmedOrUnknown), Err(CancellationError::MissingAttempt));
    let ledger = EntityId::new();
    let attempt = LocalAttempt { request_id: sent_id, provider_id: "anthropic".into(), account_scope_ref: "sub".into(),
        window_key: "api:requests".into(), unit: "requests".into(), ledger_commit: CommitRef::new(ledger, LedgerSeq(3)),
        cost: Range::point(1.0).unwrap(), billing: AttemptBilling::Unknown };
    let armed = mark_armed(&mut sent_pacer, &sent, &[attempt.clone()]).unwrap();
    let saved = serde_json::to_string(&sent_pacer).unwrap();
    sent_pacer = serde_json::from_str(&saved).unwrap();
    let (_, retained) = cancel(&mut sent_pacer, &armed, DispatchEvidence::ArmedOrUnknown).unwrap();
    assert!(retained.iter().any(|cost| cost.window == window && Some(cost.amount) == Range::new(0.0, 1.0)));
    let waiting_id = EntityId::new();
    assert!(matches!(admit(&p, &decision, waiting_id, &mut sent_pacer, &available, now()),
        AdmissionOutcome::Quota(admission) if admission.state == AdmissionState::QuotaPolicyWait));

    let unrelated = LocalAttempt { ledger_commit: CommitRef::new(EntityId::new(), LedgerSeq(3)), ..attempt.clone() };
    snapshot.covered_through = Some(CoveredThrough { local_ledger_commit: unrelated.ledger_commit });
    let wrong = forecast(&snapshot, &[unrelated], &HashMap::new(), now(), options);
    let wrong_decision = decide(&p, &[wrong], &available, None, now()).unwrap();
    assert!(matches!(admit(&p, &wrong_decision, waiting_id, &mut sent_pacer, &available, now()),
        AdmissionOutcome::Quota(admission) if admission.state == AdmissionState::QuotaPolicyWait),
        "same request and sequence in a different ledger must not release the bound reservation");
    snapshot.covered_through = Some(CoveredThrough { local_ledger_commit: CommitRef::new(ledger, LedgerSeq(3)) });
    let covered = forecast(&snapshot, &[attempt.clone()], &HashMap::new(), now(), options);
    assert!(covered.windows[0].accounted_attempts.iter().any(|a| a.request_id == sent_id && a.covered));
    let next = decide(&p, &[covered], &available, None, now()).unwrap();
    assert!(matches!(admit(&p, &next, waiting_id, &mut sent_pacer, &available, now()),
        AdmissionOutcome::Quota(admission) if admission.state == AdmissionState::Admitted));

    // A committed attempt already subtracted by the forecast must not be
    // subtracted a second time as a still-outstanding local reservation.
    let larger = from_api_headers(&ctx, [
        ("anthropic-ratelimit-requests-limit", "2"),
        ("anthropic-ratelimit-requests-remaining", "2"),
    ]);
    let original = forecast(&larger, &[], &HashMap::new(), now(), options);
    let decision = decide(&p, &[original], &available, None, now()).unwrap();
    let mut pacer = Pacer::default();
    let AdmissionOutcome::Quota(first) = admit(&p, &decision, sent_id, &mut pacer, &available, now()) else { panic!("first must admit") };
    mark_armed(&mut pacer, &first, &[attempt.clone()]).unwrap();
    let duplicate = forecast(&larger, &[attempt.clone(), attempt.clone()], &HashMap::new(), now(), options);
    assert!(matches!(duplicate.windows[0].planning, Planning::Unknown { .. }));
    let unreflected = forecast(&larger, &[attempt.clone()], &HashMap::new(), now(), options);
    assert!(!unreflected.windows[0].accounted_attempts[0].covered);
    let decision = decide(&p, &[unreflected], &available, None, now()).unwrap();
    assert!(matches!(admit(&p, &decision, EntityId::new(), &mut pacer, &available, now()),
        AdmissionOutcome::Quota(admission) if admission.state == AdmissionState::Admitted));

    // A newer covered observation must not free a charge for a decision
    // frozen against the older, pre-charge snapshot.
    let old_snapshot = from_api_headers(&ctx, [
        ("anthropic-ratelimit-requests-limit", "1"),
        ("anthropic-ratelimit-requests-remaining", "1"),
    ]);
    let old_forecast = forecast(&old_snapshot, &[], &HashMap::new(), now(), options);
    let old_decision = decide(&p, &[old_forecast], &available, None, now()).unwrap();
    let mut stale_pacer = Pacer::default();
    let old_id = EntityId::new();
    let AdmissionOutcome::Quota(old_admission) = admit(&p, &old_decision, old_id, &mut stale_pacer, &available, now()) else { panic!("initial admission expected") };
    let old_attempt = LocalAttempt { request_id: old_id, ledger_commit: CommitRef::new(EntityId::new(), LedgerSeq(5)), ..attempt.clone() };
    mark_armed(&mut stale_pacer, &old_admission, &[old_attempt.clone()]).unwrap();
    let later = now() + Duration::seconds(1);
    let later_ctx = ObservationContext { observed_at: later, ..ctx.clone() };
    let mut spent = from_api_headers(&later_ctx, [
        ("anthropic-ratelimit-requests-limit", "1"),
        ("anthropic-ratelimit-requests-remaining", "0"),
    ]);
    spent.covered_through = Some(CoveredThrough { local_ledger_commit: old_attempt.ledger_commit });
    let newer = forecast(&spent, &[old_attempt], &HashMap::new(), later, options);
    let _newer_decision = decide(&p, &[newer], &available, None, later).unwrap();
    assert!(!matches!(admit(&p, &old_decision, EntityId::new(), &mut stale_pacer, &available, later),
        AdmissionOutcome::Quota(admission) if admission.state == AdmissionState::Admitted),
        "a prior decision cannot reuse a charge removed by a newer coverage observation");
}

#[test]
fn persisted_fractional_policy_preserves_its_bound_decision() {
    let mut p = single_window_policy("five_hour", "percent", &[("cloud", 1.0)]);
    p.min_progress_per_hour = 51.248178375505404;
    let forecast = forecast_of(vec![("five_hour", period(100.0, 10.0, 10), Range::point(100.0))]);
    let available = availability(&p, false);
    let decision = decide(&p, &[forecast], &available, None, now()).unwrap();
    let reopened: QuotaPolicy = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
    let decision: PolicyDecision = serde_json::from_str(&serde_json::to_string(&decision).unwrap()).unwrap();
    assert_eq!(reopened.min_progress_per_hour.to_bits(), p.min_progress_per_hour.to_bits());
    assert!(matches!(admit(&reopened, &decision, EntityId::new(), &mut Pacer::default(), &available, now()),
        AdmissionOutcome::Quota(record) if record.state == AdmissionState::Admitted));
}

#[test]
fn source_freshness_expires_both_forecasts_and_decisions() {
    let p = single_window_policy("api:requests", "requests", &[("cloud", 1.0)]);
    let available = availability(&p, false);
    let ctx = ObservationContext { provider_id: "anthropic".into(), account_scope_ref: "sub".into(),
        source_version: None, attribution: Attribution::Exclusive, observed_at: now() };
    let snapshot = from_api_headers(&ctx, [("anthropic-ratelimit-requests-limit", "1"),
        ("anthropic-ratelimit-requests-remaining", "1")]);
    let options = ForecastOptions { ttl: Duration::minutes(5), stale: StaleUse::Refuse };
    let original = forecast(&snapshot, &[], &HashMap::new(), now(), options);
    let decision = decide(&p, &[original.clone()], &available, None, now()).unwrap();
    let later = now() + Duration::minutes(6);
    assert!(matches!(admit(&p, &decision, EntityId::new(), &mut Pacer::default(), &available, later),
        AdmissionOutcome::Quota(record) if record.state == AdmissionState::Eligible));
    let expired = decide(&p, &[original], &available, None, later).unwrap();
    assert!(matches!(expired.choice, Choice::ObservationBlocked { .. }));
    let last_known = forecast(&snapshot, &[], &HashMap::new(), later,
        ForecastOptions { stale: StaleUse::LastKnown, ..options });
    assert_eq!(last_known.freshness, Freshness::Stale);
    assert!(matches!(decide(&p, &[last_known], &available, None, later).unwrap().choice, Choice::Cloud { .. }));
}

#[test]
fn profile_switch_preserves_the_prior_request_pacing_debt() {
    let p = single_window_policy("five_hour", "percent", &[("expensive", 90.0), ("cheap", 1.0)]);
    let f = forecast_of(vec![("five_hour", period(100.0, 10.0, 10), Range::point(100.0))]);
    let mut available = availability(&p, false);
    let expensive = decide(&p, &[f.clone()], &available, None, now()).unwrap();
    let mut pacer = Pacer::default();
    assert!(matches!(admit(&p, &expensive, EntityId::new(), &mut pacer, &available, now()),
        AdmissionOutcome::Quota(record) if record.state == AdmissionState::Admitted));
    available.insert("expensive".into(), BackendState::Unavailable);
    let later = now() + Duration::minutes(7);
    let cheap = decide(&p, &[f], &available, None, later).unwrap();
    assert!(matches!(admit(&p, &cheap, EntityId::new(), &mut pacer, &available, later),
        AdmissionOutcome::Quota(record) if record.state == AdmissionState::QuotaPolicyWait));
}

#[test]
fn corrected_period_end_does_not_erase_pacing_debt() {
    let p = single_window_policy("five_hour", "percent", &[("cloud", 1.0)]);
    let old = forecast_of(vec![("five_hour", period(100.0, 10.0, 10), Range::point(100.0))]);
    let available = availability(&p, false);
    let first = decide(&p, &[old], &available, None, now()).unwrap();
    let mut pacer = Pacer::default();
    assert!(matches!(admit(&p, &first, EntityId::new(), &mut pacer, &available, now()),
        AdmissionOutcome::Quota(record) if record.state == AdmissionState::Admitted));
    let later = now() + Duration::minutes(1);
    let changed = forecast_of(vec![("five_hour", Planning::Period { available: 100.0, per_hour: 10.0,
        resets_at: now() + Duration::hours(10) - Duration::minutes(1) }, Range::point(100.0))]);
    let corrected = decide(&p, &[changed], &available, None, later).unwrap();
    assert!(matches!(admit(&p, &corrected, EntityId::new(), &mut pacer, &available, later),
        AdmissionOutcome::Quota(record) if record.state == AdmissionState::QuotaPolicyWait));
}

#[test]
fn losing_selected_backend_does_not_hide_an_available_fallback() {
    let p = single_window_policy("five_hour", "percent", &[("smart", 2.0), ("cheap", 1.0)]);
    let f = forecast_of(vec![("five_hour", period(100.0, 10.0, 10), Range::point(100.0))]);
    let mut available = availability(&p, false);
    let original = decide(&p, &[f.clone()], &available, None, now()).unwrap();
    available.insert("smart".into(), BackendState::Unavailable);
    let mut pacer = Pacer::default();
    assert!(matches!(admit(&p, &original, EntityId::new(), &mut pacer, &available, now()),
        AdmissionOutcome::Quota(record) if record.state == AdmissionState::Eligible));
    let fallback = decide(&p, &[f], &available, None, now()).unwrap();
    assert_eq!(cloud(&fallback).0, "cheap");
    assert!(matches!(admit(&p, &fallback, EntityId::new(), &mut pacer, &available, now()),
        AdmissionOutcome::Quota(record) if record.state == AdmissionState::Admitted));
}

#[test]
fn unknown_backend_is_not_hidden_by_an_unusable_available_one() {
    let p = single_window_policy("five_hour", "percent", &[("smart", 2.0), ("cheap", 1.0)]);
    let available = HashMap::from([("smart".into(), BackendState::Available), ("cheap".into(), BackendState::Unknown)]);
    let decision = decide(&p, &[], &available, None, now()).unwrap();
    assert!(matches!(decision.choice, Choice::CapacityUnknown { .. }));
    assert!(matches!(admit(&p, &decision, EntityId::new(), &mut Pacer::default(), &available, now()),
        AdmissionOutcome::CapacityUnknown { .. }));
}

#[test]
fn quota_pacing_handles_tiny_rates_and_clock_skew() {
    let mut p = policy();
    p.profiles.truncate(1);
    p.profiles[0].cost_per_request.truncate(1);
    p.min_progress_per_hour = 0.0;
    let available = availability(&p, false);
    let mut f = forecast_of(vec![("five_hour", period(1.0, 1e-300, 2), Range::point(1.0))]);
    // The profile's first cost must fit the available amount.
    p.profiles[0].cost_per_request[0].amount = Range::point(1.0).unwrap();
    let decision = decide(&p, &[f.clone()], &available, None, now()).unwrap();
    let mut pacer = Pacer::default();
    assert!(matches!(admit(&p, &decision, EntityId::new(), &mut pacer, &available, now()),
        AdmissionOutcome::Quota(admission) if admission.state == AdmissionState::Admitted));
    assert!(matches!(admit(&p, &decision, EntityId::new(), &mut pacer, &available, now()),
        AdmissionOutcome::Quota(admission) if admission.state == AdmissionState::QuotaPolicyWait && admission.next_eligible_at.is_none()));
    let mut future = decision.clone();
    future.decided_at = now() + Duration::seconds(1);
    assert!(matches!(admit(&p, &future, EntityId::new(), &mut Pacer::default(), &available, now()),
        AdmissionOutcome::Quota(admission) if admission.state == AdmissionState::Eligible));
    f.computed_at = now() + Duration::seconds(1);
    assert!(matches!(decide(&p, &[f], &available, None, now()).unwrap().choice,
        Choice::ObservationBlocked { .. }));
}

#[test]
fn unrelated_quota_windows_do_not_share_a_pacing_clock() {
    let mut p = policy();
    let cost = |window| WindowCost { window: wref(window), unit: "percent".into(), amount: Range::point(1.0).unwrap() };
    p.profiles = vec![
        ModelProfile { profile_id: "a".into(), model_ref: "model/a".into(), local: false, cost_per_request: vec![cost("five_hour")] },
        ModelProfile { profile_id: "b".into(), model_ref: "model/b".into(), local: false, cost_per_request: vec![cost("seven_day")] },
    ];
    p.min_progress_per_hour = 0.0;
    let f = forecast_of(vec![
        ("five_hour", period(10.0, 1.0, 10), Range::point(10.0)),
        ("seven_day", period(10.0, 1.0, 10), Range::point(10.0)),
    ]);
    let only_a = HashMap::from([("a".into(), BackendState::Available), ("b".into(), BackendState::Unavailable)]);
    let only_b = HashMap::from([("a".into(), BackendState::Unavailable), ("b".into(), BackendState::Available)]);
    let mut pacer = Pacer::default();
    let a = decide(&p, &[f.clone()], &only_a, None, now()).unwrap();
    assert!(matches!(admit(&p, &a, EntityId::new(), &mut pacer, &only_a, now()),
        AdmissionOutcome::Quota(admission) if admission.state == AdmissionState::Admitted));
    let mut broken = serde_json::to_value(&pacer).unwrap();
    broken["pacing"] = serde_json::json!([]);
    assert!(serde_json::from_value::<Pacer>(broken).is_err(), "missing pacing state cannot silently restore");
    let b = decide(&p, &[f.clone()], &only_b, None, now()).unwrap();
    assert!(matches!(admit(&p, &b, EntityId::new(), &mut pacer, &only_b, now()),
        AdmissionOutcome::Quota(admission) if admission.state == AdmissionState::Admitted));
    let next_now = now() + Duration::hours(10) + Duration::seconds(1);
    let mut next = f;
    next.forecast_id = EntityId::new();
    next.based_on_snapshot = EntityId::new();
    next.computed_at = next_now;
    for window in &mut next.windows {
        if let Planning::Period { resets_at, .. } = &mut window.planning {
            *resets_at = next_now + Duration::hours(10);
        }
    }
    let a = decide(&p, &[next], &only_a, None, next_now).unwrap();
    assert!(matches!(admit(&p, &a, EntityId::new(), &mut pacer, &only_a, next_now),
        AdmissionOutcome::Quota(admission) if admission.state == AdmissionState::Admitted),
        "a newly observed period must not inherit an old period's pacing clock");
}
