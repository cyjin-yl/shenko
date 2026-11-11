//! The replaceable quota-strategy seam — docs/specs/quota-resources.md#spec-42-1, docs/specs/quota-resources.md#spec-42-5, docs/specs/quota-resources.md#spec-42-6.
//!
//! docs/specs/quota-resources.md#spec-42-1 makes the forecaster, the routing policy and the admission cadence
//! replaceable plugins. These are library-level consumers of the public API:
//! they install a strategy, select it, and check that the produced record
//! names the strategy while the durable state machine keeps every safety
//! property it had before. All provider figures are synthetic headers; no
//! account, provider or model is observed here.
use std::collections::HashMap;

use chrono::{DateTime, Duration, TimeZone, Utc};
use shenko_quota::*;
use shenko_types::{
    commit::CommitRef,
    ids::{EntityId, LedgerSeq},
};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 27, 12, 0, 0).unwrap()
}

fn wref(key: &str) -> WindowRef {
    WindowRef { provider_id: "anthropic".into(), account_scope_ref: "sub".into(), window_key: key.into() }
}

fn descriptor(kind: StrategyKind, id: &str, version: &str) -> StrategyDescriptor {
    StrategyDescriptor {
        protocol_version: QUOTA_STRATEGY_PROTOCOL_VERSION,
        strategy_id: id.into(),
        version: version.into(),
        kind,
    }
}

/// One synthetic API reading of the request bucket.
fn request_snapshot(remaining: &str, observed_at: DateTime<Utc>) -> QuotaSnapshot {
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

/// One cloud profile costing one request of the only window.
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
        switching_hysteresis: 0.2,
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

fn availability(policy: &QuotaPolicy, state: BackendState) -> HashMap<String, BackendState> {
    policy.profiles.iter().map(|profile| (profile.profile_id.clone(), state)).collect()
}

fn request_forecast(snapshot: &QuotaSnapshot) -> QuotaForecast {
    forecast(snapshot, &[], &HashMap::new(), now(), options())
}

/// A forecaster that keeps the reading but publishes no plan at all: the
/// replacement is visible in the record without inventing a balance.
struct ScriptedForecaster;

impl QuotaForecaster for ScriptedForecaster {
    fn descriptor(&self) -> StrategyDescriptor {
        descriptor(StrategyKind::Forecast, "test.scripted_forecast", "1")
    }

    fn forecast(&self, request: ForecastRequest<'_>) -> Result<QuotaForecast, StrategyError> {
        let mut produced = RollingBaseline.forecast(request)?;
        for window in &mut produced.windows {
            window.planning = Planning::Unknown { reason: "scripted: no plan published".into() };
        }
        Ok(produced)
    }
}

/// A forecaster that declines instead of guessing.
struct RefusingForecaster;

impl QuotaForecaster for RefusingForecaster {
    fn descriptor(&self) -> StrategyDescriptor {
        descriptor(StrategyKind::Forecast, "test.refusing_forecast", "1")
    }

    fn forecast(&self, _request: ForecastRequest<'_>) -> Result<QuotaForecast, StrategyError> {
        Err(StrategyError::Refused { reason: "no admitted reading".into() })
    }
}

/// A router that holds every request until the next period boundary. The
/// durable admission path must accept it as a wait, never as budget.
struct HoldingRouter;

impl QuotaRouter for HoldingRouter {
    fn descriptor(&self) -> StrategyDescriptor {
        descriptor(StrategyKind::Route, "test.holding_router", "1")
    }

    fn route(&self, request: RouteRequest<'_>) -> Result<PolicyDecision, StrategyError> {
        let mut produced = RollingBaseline.route(request)?;
        produced.choice = Choice::Wait { until: Some(now() + Duration::minutes(10)) };
        produced.constraints = Vec::new();
        Ok(produced)
    }
}

/// A cadence strategy that protects a reserve for this send.
struct DeferringCadence {
    until: Option<DateTime<Utc>>,
}

impl QuotaCadence for DeferringCadence {
    fn descriptor(&self) -> StrategyDescriptor {
        descriptor(StrategyKind::Cadence, "test.deferring_cadence", "1")
    }

    fn plan(&self, _request: CadenceRequest<'_>) -> Result<CadencePlan, StrategyError> {
        Ok(CadencePlan::Defer {
            until: self.until,
            reason: "protect the period reserve".into(),
        })
    }
}

fn strategies_with_cadence(until: Option<DateTime<Utc>>) -> QuotaStrategies {
    let mut registry = QuotaStrategyRegistry::new();
    registry.register_cadence(Box::new(DeferringCadence { until })).unwrap();
    registry.select(StrategyKind::Cadence, "test.deferring_cadence").unwrap();
    QuotaStrategies::from_registry(registry)
}

/// The zero-plugin configuration is exactly the pre-existing behavior, and
/// the record says which strategy produced the numbers.
#[test]
fn baseline_strategy_reproduces_the_pure_functions_and_is_attributed() {
    let snapshot = request_snapshot("6", now());
    let pure = request_forecast(&snapshot);
    let baseline = QuotaStrategies::baseline();
    let produced = baseline
        .forecast(ForecastRequest {
            snapshot: &snapshot,
            attempts: &[],
            assumptions: &HashMap::new(),
            now: now(),
            options: options(),
        })
        .unwrap();
    assert_eq!(
        produced.cost_model_version.as_deref(),
        Some("shenko.quota.rolling_baseline@1")
    );
    assert_eq!(
        forecast_material(&pure),
        forecast_material(&produced),
        "the baseline plugin is the same algorithm, not a second one"
    );
    assert!(pure.cost_model_version.is_none());
    // Attribution is additive: a record produced without the seam keeps no
    // key at all, so a legacy row still round-trips byte for byte.
    assert!(!serde_json::to_string(&pure).unwrap().contains("cost_model_version"));
    assert!(serde_json::to_string(&produced).unwrap().contains("shenko.quota.rolling_baseline@1"));

    // The same for the routing strategy.
    let policy = policy();
    let available = availability(&policy, BackendState::Available);
    let pure_decision = decide(&policy, &[pure.clone()], &available, None, now()).unwrap();
    assert!(pure_decision.strategy.is_none());
    let routed = baseline
        .route(RouteRequest {
            policy: &policy,
            forecasts: &[pure],
            availability: &available,
            previous: None,
            work: None,
            now: now(),
        })
        .unwrap();
    assert_eq!(routed.strategy.as_deref(), Some("shenko.quota.rolling_baseline@1"));
    assert_eq!(routed.choice, pure_decision.choice);

    // And for admission: the durable record is identical under the baseline.
    let request_id = EntityId::new();
    let mut pure_pacer = Pacer::default();
    let mut facade_pacer = Pacer::default();
    let AdmissionOutcome::Quota(pure_admission) =
        admit(&policy, &pure_decision, request_id, &mut pure_pacer, &available, now())
    else {
        panic!("expected an admitted record");
    };
    let AdmissionOutcome::Quota(facade_admission) = baseline
        .admit(&policy, &pure_decision, request_id, &mut facade_pacer, &available, now())
        .unwrap()
    else {
        panic!("expected an admitted record through the strategy layer");
    };
    assert_eq!(facade_admission.state, pure_admission.state);
    assert_eq!(facade_admission.estimated_cost, pure_admission.estimated_cost);
    // Admission ids are freshly minted per distinct request record, so the
    // durable substance is compared instead of the identity.
    assert_eq!(
        facade_pacer.reservation(&request_id).and_then(|r| r.dispatch),
        pure_pacer.reservation(&request_id).and_then(|r| r.dispatch)
    );
    assert_eq!(
        facade_pacer.reservation(&request_id).map(|r| r.outstanding.clone()),
        pure_pacer.reservation(&request_id).map(|r| r.outstanding.clone())
    );
}

/// An installed forecast strategy is used, its record names it, and a
/// refusal is returned as such instead of a fabricated forecast.
#[test]
fn replacement_forecaster_is_used_and_refusal_fabricates_nothing() {
    let snapshot = request_snapshot("6", now());
    let mut registry = QuotaStrategyRegistry::new();
    registry.register_forecaster(Box::new(ScriptedForecaster)).unwrap();
    registry.select(StrategyKind::Forecast, "test.scripted_forecast").unwrap();
    let strategies = QuotaStrategies::from_registry(registry);
    let produced = strategies
        .forecast(ForecastRequest {
            snapshot: &snapshot,
            attempts: &[],
            assumptions: &HashMap::new(),
            now: now(),
            options: options(),
        })
        .unwrap();
    assert_eq!(
        produced.cost_model_version.as_deref(),
        Some("test.scripted_forecast@1")
    );
    assert!(produced
        .windows
        .iter()
        .all(|window| matches!(&window.planning, Planning::Unknown { .. })));
    // The reading itself is still the recorded evidence, not the plugin's.
    assert_eq!(produced.based_on_snapshot, snapshot.snapshot_id);

    let mut registry = QuotaStrategyRegistry::new();
    registry.register_forecaster(Box::new(RefusingForecaster)).unwrap();
    registry.select(StrategyKind::Forecast, "test.refusing_forecast").unwrap();
    let refusing = QuotaStrategies::from_registry(registry);
    let error = refusing
        .forecast(ForecastRequest {
            snapshot: &snapshot,
            attempts: &[],
            assumptions: &HashMap::new(),
            now: now(),
            options: options(),
        })
        .expect_err("a refusal is never a forecast");
    assert_eq!(error, StrategyError::Refused { reason: "no admitted reading".into() });
    // A refused forecast leaves no record to admit against.
    assert!(error.to_string().contains("no admitted reading"));
}

/// A replacement router is used, and the durable admission path still
/// resolves its decision under the recorded policy and forecasts.
#[test]
fn replacement_router_decision_flows_through_durable_admission() {
    let snapshot = request_snapshot("6", now());
    let forecast = request_forecast(&snapshot);
    let policy = policy();
    let available = availability(&policy, BackendState::Available);
    let baseline_decision = decide(&policy, &[forecast.clone()], &available, None, now()).unwrap();
    assert!(matches!(&baseline_decision.choice, Choice::Cloud { .. }));

    let mut registry = QuotaStrategyRegistry::new();
    registry.register_router(Box::new(HoldingRouter)).unwrap();
    registry.select(StrategyKind::Route, "test.holding_router").unwrap();
    let strategies = QuotaStrategies::from_registry(registry);
    let routed = strategies
        .route(RouteRequest {
            policy: &policy,
            forecasts: &[forecast],
            availability: &available,
            previous: None,
            work: None,
            now: now(),
        })
        .unwrap();
    assert_eq!(routed.strategy.as_deref(), Some("test.holding_router@1"));
    assert_eq!(
        routed.choice,
        Choice::Wait { until: Some(now() + Duration::minutes(10)) }
    );

    // The durable transition honors the wait and reserves no budget.
    let request_id = EntityId::new();
    let mut pacer = Pacer::default();
    let AdmissionOutcome::Quota(held) = strategies
        .admit(&policy, &routed, request_id, &mut pacer, &available, now())
        .unwrap()
    else {
        panic!("expected a held admission record");
    };
    assert_eq!(held.state, AdmissionState::QuotaPolicyWait);
    assert_eq!(held.next_eligible_at, Some(now() + Duration::minutes(10)));
    assert!(held.estimated_cost.is_empty());
    let reservation = pacer.reservation(&request_id).expect("the wait is durable, not a phantom");
    assert!(reservation.outstanding.is_empty());
    assert!(reservation.dispatch.is_none());

    // A decision that no longer matches the recorded policy is still
    // rejected by the library: it becomes a "recompute before admitting"
    // record, never an admission, so a replacement router cannot authorize a
    // send on its own.
    let mut mismatched = routed.clone();
    mismatched.policy_revision = 999;
    let mut mismatched_pacer = Pacer::default();
    let AdmissionOutcome::Quota(rejected) = strategies
        .admit(&policy, &mismatched, EntityId::new(), &mut mismatched_pacer, &available, now())
        .unwrap()
    else {
        panic!("a stale policy reference is not an admission");
    };
    assert_eq!(rejected.state, AdmissionState::Eligible);
    assert!(rejected.estimated_cost.is_empty());
}

/// A cadence strategy may withhold a fresh admission only: it cannot spend
/// budget, downgrade armed evidence or mask provider exhaustion.
#[test]
fn cadence_withholds_fresh_admissions_without_spending_or_rewriting_history() {
    let snapshot = request_snapshot("6", now());
    let forecast = request_forecast(&snapshot);
    let policy = policy();
    let available = availability(&policy, BackendState::Available);
    let decision = decide(&policy, &[forecast], &available, None, now()).unwrap();
    let until = now() + Duration::minutes(4);
    let strategies = strategies_with_cadence(Some(until));
    let request_id = EntityId::new();

    let mut pacer = Pacer::default();
    let AdmissionOutcome::Quota(held) = strategies
        .admit(&policy, &decision, request_id, &mut pacer, &available, now())
        .unwrap()
    else {
        panic!("expected a withheld admission record");
    };
    assert_eq!(held.state, AdmissionState::QuotaPolicyWait);
    assert_eq!(held.next_eligible_at, Some(until));
    assert!(held.reason.contains("cadence deferred"));
    let reservation = pacer.reservation(&request_id).expect("the deferral is durable");
    assert!(reservation.outstanding.is_empty(), "a deferral reserves no budget");
    assert!(reservation.admission.estimated_cost.is_empty());
    assert!(reservation.dispatch.is_none());
    assert!(reservation.attempts.is_empty(), "nothing was armed");

    // A deadline that is not still ahead means "re-evaluate on new
    // evidence", never a fabricated past wait.
    let stale_deferral = strategies_with_cadence(Some(now() - Duration::minutes(1)));
    let mut fresh_pacer = Pacer::default();
    let AdmissionOutcome::Quota(soon) = stale_deferral
        .admit(&policy, &decision, EntityId::new(), &mut fresh_pacer, &available, now())
        .unwrap()
    else {
        panic!("expected a withheld admission record");
    };
    assert_eq!(soon.state, AdmissionState::QuotaPolicyWait);
    assert_eq!(soon.next_eligible_at, None);

    // Preserved armed history outranks a cadence opinion: arming under the
    // baseline and then re-admitting with the same decision keeps the bound
    // attempt rows and the armed charge.
    let mut armed_pacer = Pacer::default();
    let AdmissionOutcome::Quota(admitted) =
        QuotaStrategies::baseline().admit(
            &policy,
            &decision,
            request_id,
            &mut armed_pacer,
            &available,
            now(),
        )
        .unwrap()
    else {
        panic!("baseline admits within budget");
    };
    let commit = CommitRef::new(EntityId::new(), LedgerSeq(1));
    let attempts: Vec<LocalAttempt> = admitted
        .estimated_cost
        .iter()
        .map(|cost| LocalAttempt {
            request_id: admitted.request_id,
            provider_id: cost.window.provider_id.clone(),
            account_scope_ref: cost.window.account_scope_ref.clone(),
            window_key: cost.window.window_key.clone(),
            unit: cost.unit.clone(),
            ledger_commit: commit,
            cost: cost.amount,
            billing: AttemptBilling::Unknown,
        })
        .collect();
    let armed = mark_armed(&mut armed_pacer, &admitted, &attempts).unwrap();
    let after = strategies
        .admit(&policy, &decision, request_id, &mut armed_pacer, &available, now())
        .unwrap();
    let AdmissionOutcome::Existing { admission } = after else {
        panic!("an armed send is never re-decided into a wait");
    };
    assert_eq!(admission.state, AdmissionState::Admitted);
    assert_eq!(admission.admission_id, armed.admission_id);
    let reservation = armed_pacer.reservation(&request_id).unwrap();
    assert_eq!(reservation.dispatch, Some(DispatchEvidence::ArmedOrUnknown));
    assert!(!reservation.outstanding.is_empty(), "the armed charge survives");

    // Provider exhaustion is a fact a cadence cannot turn into a wait.
    let exhausted_snapshot = request_snapshot("0", now());
    let exhausted_forecast = request_forecast(&exhausted_snapshot);
    let exhausted = decide(&policy, &[exhausted_forecast], &available, None, now()).unwrap();
    assert!(matches!(&exhausted.choice, Choice::ProviderExhausted { .. }));
    let mut exhausted_pacer = Pacer::default();
    let outcome = strategies
        .admit(&policy, &exhausted, EntityId::new(), &mut exhausted_pacer, &available, now())
        .unwrap();
    let AdmissionOutcome::Quota(record) = outcome else {
        panic!("expected a quota exhaustion record");
    };
    assert_eq!(record.state, AdmissionState::ProviderExhausted);
}

/// Registration and selection protect the identity written into records.
#[test]
fn strategy_registration_refuses_ambiguous_or_reserved_identities() {
    let mut registry = QuotaStrategyRegistry::new();
    // Another protocol version is refused, so field meanings cannot drift.
    let mut wrong_version = descriptor(StrategyKind::Forecast, "test.wrong_version", "1");
    wrong_version.protocol_version = QUOTA_STRATEGY_PROTOCOL_VERSION + 1;
    struct Fixed(StrategyDescriptor);
    impl QuotaForecaster for Fixed {
        fn descriptor(&self) -> StrategyDescriptor {
            self.0.clone()
        }
        fn forecast(&self, request: ForecastRequest<'_>) -> Result<QuotaForecast, StrategyError> {
            RollingBaseline.forecast(request)
        }
    }
    assert_eq!(
        registry.register_forecaster(Box::new(Fixed(wrong_version))),
        Err(StrategyError::ProtocolMismatch)
    );
    // An empty or padded identifier is not a stable name.
    assert_eq!(
        registry.register_forecaster(Box::new(Fixed(descriptor(
            StrategyKind::Forecast,
            " ",
            "1"
        )))),
        Err(StrategyError::InvalidDescriptor)
    );
    // The descriptor's kind must match the slot it is registered into.
    assert_eq!(
        registry.register_forecaster(Box::new(Fixed(descriptor(
            StrategyKind::Route,
            "test.wrong_kind",
            "1"
        )))),
        Err(StrategyError::KindMismatch)
    );
    // The baseline's own name is reserved for the baseline.
    assert_eq!(
        registry.register_forecaster(Box::new(Fixed(descriptor(
            StrategyKind::Forecast,
            BASELINE_STRATEGY_ID,
            "2"
        )))),
        Err(StrategyError::ReservedIdentifier)
    );
    assert!(registry.installed().iter().all(|item| item.strategy_id == BASELINE_STRATEGY_ID));

    // A valid install is accepted once, and only a selected strategy is used.
    registry
        .register_forecaster(Box::new(Fixed(descriptor(
            StrategyKind::Forecast,
            "test.accepted",
            "1",
        ))))
        .unwrap();
    assert_eq!(
        registry.register_forecaster(Box::new(Fixed(descriptor(
            StrategyKind::Forecast,
            "test.accepted",
            "2"
        )))),
        Err(StrategyError::DuplicateStrategy)
    );
    assert_eq!(
        registry.forecaster().descriptor().strategy_id,
        BASELINE_STRATEGY_ID,
        "installing does not activate"
    );
    assert_eq!(
        registry.select(StrategyKind::Forecast, "test.missing"),
        Err(StrategyError::UnknownStrategy)
    );
    registry.select(StrategyKind::Forecast, "test.accepted").unwrap();
    assert_eq!(registry.forecaster().descriptor().strategy_id, "test.accepted");
    let described = registry.installed();
    assert_eq!(described[0].strategy_id, "test.accepted");
    assert!(described.iter().any(|item| item.strategy_id == BASELINE_STRATEGY_ID));
    // Selecting the reserved baseline name returns to the built-in baseline.
    registry.select(StrategyKind::Forecast, BASELINE_STRATEGY_ID).unwrap();
    assert_eq!(registry.forecaster().descriptor().strategy_id, BASELINE_STRATEGY_ID);
}

/// The strategy layer never upgrades an old reading: a stale source keeps
/// its unknown plan and its staleness, it is not "full" or "exhausted".
#[test]
fn stale_evidence_keeps_its_unknown_plan_through_the_strategy_layer() {
    let stale_at = now() - Duration::minutes(10);
    let snapshot = request_snapshot("6", stale_at);
    let baseline = QuotaStrategies::baseline();
    let produced = baseline
        .forecast(ForecastRequest {
            snapshot: &snapshot,
            attempts: &[],
            assumptions: &HashMap::new(),
            now: now(),
            options: options(),
        })
        .unwrap();
    assert_ne!(produced.freshness, Freshness::Fresh);
    assert_eq!(produced.cost_model_version.as_deref(), Some("shenko.quota.rolling_baseline@1"));
    assert!(
        produced
            .windows
            .iter()
            .all(|window| matches!(&window.planning, Planning::Unknown { .. })),
        "a source past its lifetime cannot plan, whatever strategy reads it"
    );
    // The reading itself is preserved as evidence, with its own timestamp.
    assert_eq!(produced.computed_at, now());
    assert_eq!(produced.windows[0].basis, produced.freshness);
}
