//! docs/specs/quota-resources.md#spec-42-3 / QA05 planning targets, holds and pacing on synthetic readings.
//!
//! Every provider figure here is a synthetic API-header reading. These are
//! library-level consumers: they show that the new policy fields change the
//! planner (rate, ceiling, validity), that nothing is invented where the
//! evidence is unknown, and that a legacy row re-serializes to its recorded
//! bytes so old digests and armed reservations stay valid.
use std::collections::HashMap;

use chrono::{DateTime, Duration, TimeZone, Utc};
use shenko_quota::*;
use shenko_types::ids::EntityId;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 27, 12, 0, 0).unwrap()
}

fn wref(key: &str) -> WindowRef {
    WindowRef {
        provider_id: "anthropic".into(),
        account_scope_ref: "sub".into(),
        window_key: key.into(),
    }
}

fn snapshot(remaining: &str, observed_at: DateTime<Utc>) -> QuotaSnapshot {
    let ctx = ObservationContext {
        provider_id: "anthropic".into(),
        account_scope_ref: "sub".into(),
        source_version: None,
        attribution: Attribution::Exclusive,
        observed_at,
    };
    from_api_headers(&ctx, [
        ("anthropic-ratelimit-requests-limit", "10"),
        ("anthropic-ratelimit-requests-remaining", remaining),
        ("anthropic-ratelimit-requests-reset", "2026-10-27T12:10:00Z"),
    ])
}

fn options() -> ForecastOptions {
    ForecastOptions { ttl: Duration::minutes(5), stale: StaleUse::Refuse }
}

/// One cloud profile costing one request of the single window.
fn policy() -> QuotaPolicy {
    QuotaPolicy {
        policy_id: EntityId::new(),
        revision: 1,
        profiles: vec![ModelProfile {
            profile_id: "cloud".into(),
            model_ref: "model/cloud".into(),
            local: false,
            cost_per_request: vec![WindowCost {
                window: wref("api:requests"),
                unit: "requests".into(),
                amount: Range::point(1.0).unwrap(),
            }],
            effort_levels: Vec::new(),
        }],
        assumptions: Vec::new(),
        min_progress_per_hour: 0.0,
        switching_hysteresis: 0.0,
        on_unknown: UnknownQuota::Block,
        allow_overage: false,
        spending_authority_ref: None,
        decision_ttl: Duration::minutes(20),
        observation_sources: Vec::new(),
        objective: PolicyObjective::Balanced,
        target_utilization_range: None,
        planning_horizon: None,
        emergency_reserve_by_window: Vec::new(),
        uncertainty_margin: None,
        admission_pacing: AdmissionPacing::default(),
    }
}

fn available(policy: &QuotaPolicy) -> HashMap<String, BackendState> {
    policy
        .profiles
        .iter()
        .map(|profile| (profile.profile_id.clone(), BackendState::Available))
        .collect()
}

fn reading(remaining: &str) -> QuotaForecast {
    forecast(&snapshot(remaining, now()), &[], &HashMap::new(), now(), options())
}

fn decided(policy: &QuotaPolicy, forecast: &QuotaForecast) -> PolicyDecision {
    decide(policy, &[forecast.clone()], &available(policy), None, now()).unwrap()
}

fn cloud_rate(decision: &PolicyDecision) -> f64 {
    match &decision.choice {
        Choice::Cloud { rate_per_hour: Some(rate), .. } => *rate,
        other => panic!("expected a paced cloud choice, got {other:?}"),
    }
}

fn admitted(policy: &QuotaPolicy, decision: &PolicyDecision, pacer: &mut Pacer, at: DateTime<Utc>) -> QuotaAdmission {
    match admit(policy, decision, EntityId::new(), pacer, &available(policy), at) {
        AdmissionOutcome::Quota(admission) => admission,
        other => panic!("expected a quota admission, got {other:?}"),
    }
}

/// The utilization target changes the planned rate; the held ceiling and the
/// derived round estimate stay the evidence's own numbers.
#[test]
fn target_utilization_scales_the_planned_rate_and_round_estimates() {
    let forecast = reading("6");
    let base = policy();
    let base_decision = decided(&base, &forecast);
    let base_rate = cloud_rate(&base_decision);
    assert_eq!(base_decision.constraints[0].available, Some(6.0));
    assert_eq!(
        base_decision.round_estimates.as_ref().and_then(|rounds| rounds.cloud_only),
        Some(6.0),
        "the held budget supports six one-request rounds"
    );
    assert_eq!(
        base_decision.round_estimates.as_ref().map(|rounds| rounds.horizon),
        Some(base.decision_ttl)
    );

    let mut targeted = policy();
    targeted.target_utilization_range = Some(Range::new(0.4, 0.5).unwrap());
    let targeted_decision = decided(&targeted, &forecast);
    let targeted_rate = cloud_rate(&targeted_decision);
    assert!(
        (targeted_rate / base_rate - 0.5).abs() < 1e-9,
        "balanced planning paces at the upper bound of the target"
    );
    assert_eq!(targeted_decision.constraints[0].available, Some(6.0));
    // The budget itself is unchanged: a target is a pace, not a smaller
    // balance.
    assert_eq!(
        targeted_decision.round_estimates.as_ref().and_then(|rounds| rounds.all_models),
        Some(6.0)
    );
    // What the planner compared is recorded with the decision.
    let scenario = targeted_decision
        .scenarios
        .iter()
        .find(|scenario| scenario.profile_id == "cloud")
        .expect("the assessed profile is recorded as a scenario");
    assert_eq!(scenario.rounds, Some(6.0));
    assert!(scenario.rate_per_hour.is_some_and(|rate| rate < base_rate));
}

/// Preserve-until-reset uses the conservative end of the target, and a
/// target objective without a target range is refused rather than silently
/// planned at the sustainable rate.
#[test]
fn preserve_uses_the_lower_bound_and_target_requires_a_range() {
    let forecast = reading("6");
    let base_rate = cloud_rate(&decided(&policy(), &forecast));

    let mut preserve = policy();
    preserve.objective = PolicyObjective::PreserveUntilReset;
    preserve.target_utilization_range = Some(Range::new(0.4, 0.5).unwrap());
    let rate = cloud_rate(&decided(&preserve, &forecast));
    assert!((rate / base_rate - 0.4).abs() < 1e-9);

    let mut target = policy();
    target.objective = PolicyObjective::TargetUtilization;
    assert_eq!(
        decide(&target, &[forecast], &available(&target), None, now()).unwrap_err(),
        PolicyError::InvalidTarget
    );
    let mut inverted = policy();
    inverted.target_utilization_range = Some(Range::new(0.9, 0.4).unwrap());
    assert_eq!(
        decide(&inverted, &[reading("6")], &available(&inverted), None, now()).unwrap_err(),
        PolicyError::InvalidTarget
    );
}

/// The planning horizon caps the decision's validity, and the emergency
/// reserve is withheld from the admission ceiling until its urgent release.
#[test]
fn planning_horizon_and_emergency_reserve_bound_the_plan() {
    let forecast = reading("6");
    let mut horizon = policy();
    horizon.planning_horizon = Some(Duration::minutes(2));
    let decision = decided(&horizon, &forecast);
    assert_eq!(decision.valid_until, now() + Duration::minutes(2));
    assert_eq!(
        decision.round_estimates.as_ref().map(|rounds| rounds.horizon),
        Some(Duration::minutes(2))
    );

    let mut reserved = policy();
    reserved.emergency_reserve_by_window = vec![WindowReserve {
        window: wref("api:requests"),
        unit: "requests".into(),
        amount: Range::point(5.0).unwrap(),
    }];
    let decision = decided(&reserved, &forecast);
    assert_eq!(
        decision.constraints[0].available,
        Some(1.0),
        "the withheld reserve is not part of the admission ceiling"
    );
    assert!(
        (cloud_rate(&decision) / cloud_rate(&decided(&policy(), &forecast)) - 1.0 / 6.0).abs() < 1e-9,
        "the paced rate is scaled by the held share"
    );
    // With only the reserve left, a further request waits for the boundary
    // instead of claiming provider exhaustion.
    let mut pacer = Pacer::default();
    let first = admitted(&reserved, &decision, &mut pacer, now());
    assert_eq!(first.state, AdmissionState::Admitted);
    let second = admitted(&reserved, &decision, &mut pacer, now() + Duration::seconds(1));
    assert_eq!(second.state, AdmissionState::QuotaPolicyWait);
    assert!(
        second
            .next_eligible_at
            .is_some_and(|at| at >= now() + Duration::minutes(9)),
        "with only the held reserve left, admission is paced to the window's rate"
    );

    // Inside the urgent window the reserve is released instead of idling.
    let mut released = reserved.clone();
    released.admission_pacing.urgent_reserve = Some(UrgentReserveRelease {
        release_within: Duration::minutes(15),
    });
    let decision = decided(&released, &forecast);
    assert_eq!(decision.constraints[0].available, Some(6.0));
}

/// A relative uncertainty margin withholds its share of the planning budget.
#[test]
fn uncertainty_margin_withholds_a_relative_share() {
    let forecast = reading("6");
    let mut cautious = policy();
    cautious.uncertainty_margin = Some(0.25);
    let decision = decided(&cautious, &forecast);
    assert_eq!(decision.constraints[0].available, Some(4.5));
    assert!(
        (cloud_rate(&decision) / cloud_rate(&decided(&policy(), &forecast)) - 0.75).abs() < 1e-9
    );
    assert_eq!(
        decision.round_estimates.as_ref().and_then(|rounds| rounds.cloud_only),
        Some(4.5)
    );

    let mut invalid = policy();
    invalid.uncertainty_margin = Some(1.5);
    assert_eq!(
        decide(&invalid, &[forecast], &available(&invalid), None, now()).unwrap_err(),
        PolicyError::InvalidReserve
    );
}

/// Pacing can be switched off (admit at the budget ceiling), a burst allows a
/// short sequence above the sustainable rate, and a minimum interval remains a
/// hard floor.
#[test]
fn pacing_switch_burst_and_minimum_interval_change_admission_spacing() {
    let forecast = reading("6");
    let paced = policy();
    let paced_decision = decided(&paced, &forecast);
    let mut pacer = Pacer::default();
    assert_eq!(admitted(&paced, &paced_decision, &mut pacer, now()).state, AdmissionState::Admitted);
    let spaced = admitted(&paced, &paced_decision, &mut pacer, now() + Duration::seconds(1));
    assert_eq!(spaced.state, AdmissionState::QuotaPolicyWait);
    assert!(
        spaced.next_eligible_at.is_some_and(|at| at > now() + Duration::seconds(90)),
        "the sustainable rate spaces admissions about 100s apart"
    );

    // Burst: two admissions may go out inside the burst window, the third is
    // paced again. The window budget still bounds them.
    let mut bursty = policy();
    bursty.admission_pacing.burst = Some(AdmissionBurst {
        max_requests: 2,
        within: Duration::minutes(1),
    });
    let burst_decision = decided(&bursty, &forecast);
    let mut pacer = Pacer::default();
    assert_eq!(admitted(&bursty, &burst_decision, &mut pacer, now()).state, AdmissionState::Admitted);
    assert_eq!(
        admitted(&bursty, &burst_decision, &mut pacer, now() + Duration::seconds(1)).state,
        AdmissionState::Admitted,
        "the burst allowance is used instead of the pacing interval"
    );
    assert_eq!(
        admitted(&bursty, &burst_decision, &mut pacer, now() + Duration::seconds(2)).state,
        AdmissionState::QuotaPolicyWait,
        "the burst is exhausted and pacing returns"
    );

    // No rate pacing at all: both requests admit at the budget ceiling.
    let mut unpaced = policy();
    unpaced.admission_pacing.enabled = false;
    let unpaced_decision = decided(&unpaced, &forecast);
    assert!(matches!(
        &unpaced_decision.choice,
        Choice::Cloud { rate_per_hour: None, .. }
    ));
    assert!(unpaced_decision.constraints.iter().all(|constraint| constraint.rate_per_hour.is_none()));
    let mut pacer = Pacer::default();
    assert_eq!(admitted(&unpaced, &unpaced_decision, &mut pacer, now()).state, AdmissionState::Admitted);
    assert_eq!(
        admitted(&unpaced, &unpaced_decision, &mut pacer, now() + Duration::seconds(1)).state,
        AdmissionState::Admitted
    );

    // A policy-wide minimum interval stays a hard floor even without pacing.
    let mut floored = unpaced.clone();
    floored.admission_pacing.minimum_interval = Some(Duration::seconds(30));
    let floor_decision = decided(&floored, &forecast);
    let mut pacer = Pacer::default();
    assert_eq!(admitted(&floored, &floor_decision, &mut pacer, now()).state, AdmissionState::Admitted);
    let held = admitted(&floored, &floor_decision, &mut pacer, now() + Duration::seconds(1));
    assert_eq!(held.state, AdmissionState::QuotaPolicyWait);
    assert_eq!(held.next_eligible_at, Some(now() + Duration::seconds(30)));
}

/// Legacy rows keep their recorded bytes: the new fields are absent from a
/// default policy, a pre-seam forecast and a pre-seam decision, so reading and
/// re-writing them cannot change a digest or invalidate an armed admission.
#[test]
fn legacy_rows_round_trip_byte_identically() {
    // Policy: a default-objective policy serializes without any new key, and
    // the struct → bytes → struct → bytes cycle is byte-identical, so the
    // content digest a recorded decision is bound to cannot move.
    let base_policy = policy();
    let policy_json = serde_json::to_string(&base_policy).unwrap();
    for absent in [
        "objective",
        "target_utilization_range",
        "planning_horizon",
        "emergency_reserve_by_window",
        "uncertainty_margin",
        "admission_pacing",
    ] {
        assert!(!policy_json.contains(absent), "{absent} must not appear on a default policy");
    }
    let reparsed: QuotaPolicy = serde_json::from_str(&policy_json).unwrap();
    assert_eq!(serde_json::to_string(&reparsed).unwrap(), policy_json);
    assert_eq!(policy_digest(&reparsed).unwrap(), policy_digest(&base_policy).unwrap());

    // A non-default field appears and its value survives the same cycle.
    let mut targeted = policy();
    targeted.target_utilization_range = Some(Range::new(0.4, 0.5).unwrap());
    targeted.planning_horizon = Some(Duration::minutes(3));
    targeted.admission_pacing.burst = Some(AdmissionBurst {
        max_requests: 2,
        within: Duration::minutes(1),
    });
    let targeted_json = serde_json::to_string(&targeted).unwrap();
    assert!(targeted_json.contains("target_utilization_range"));
    assert!(targeted_json.contains("admission_pacing"));
    let reparsed: QuotaPolicy = serde_json::from_str(&targeted_json).unwrap();
    assert_eq!(serde_json::to_string(&reparsed).unwrap(), targeted_json);
    assert_eq!(reparsed, targeted);

    // Forecast and decision: a row written before these fields existed keeps
    // exactly its own keys. The stripped JSON reads back with the honest
    // unknown defaults and re-serializes to the same content without any new
    // key appearing.
    let mut forecast_value = serde_json::to_value(reading("6")).unwrap();
    for key in ["cost_model_version", "horizon", "confidence"] {
        forecast_value.as_object_mut().unwrap().remove(key);
    }
    let reparsed: QuotaForecast = serde_json::from_value(forecast_value.clone()).unwrap();
    assert!(reparsed.cost_model_version.is_none() && reparsed.horizon.is_none());
    assert_eq!(reparsed.confidence, Confidence::Unknown);
    assert_eq!(serde_json::to_value(&reparsed).unwrap(), forecast_value);
    let forecast_text = serde_json::to_string(&reparsed).unwrap();
    for absent in ["cost_model_version", "horizon", "confidence"] {
        assert!(!forecast_text.contains(absent));
    }

    let mut decision_value = serde_json::to_value(decided(&policy(), &reading("6"))).unwrap();
    for key in ["strategy", "round_estimates", "scenarios"] {
        decision_value.as_object_mut().unwrap().remove(key);
    }
    let reparsed: PolicyDecision = serde_json::from_value(decision_value.clone()).unwrap();
    assert!(reparsed.strategy.is_none() && reparsed.round_estimates.is_none());
    assert!(reparsed.scenarios.is_empty());
    assert_eq!(serde_json::to_value(&reparsed).unwrap(), decision_value);
    let decision_text = serde_json::to_string(&reparsed).unwrap();
    for absent in ["strategy", "round_estimates", "scenarios"] {
        assert!(!decision_text.contains(absent));
    }
}
