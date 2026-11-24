//! Shared quota admission and original-send accounting for subject and finite model children.
use shenko_types::{error::ShenkoError, ids::EntityId};
use tracing::warn;

/// Quota admission gate for one sealed request.
///
/// With no recorded policy there is no gate at all: the configured or
/// unconfigured provider keeps dispatching exactly as before. Once a policy
/// exists, the request must pass durable admit → arm → step-CAS → submit;
/// quota_policy_wait / provider_exhausted / observation_blocked sleep on a
/// bounded clock and re-admit, never occupying compute or fabricating
/// inference. Unknown quota stays unknown and no request is generated to
/// probe a balance.
pub(crate) enum QuotaGate { /// Dispatch may proceed under the armed send id the CAS must claim.
/// `None` only when no quota policy is configured at all.
Ready(Option<EntityId>),
Cancelled, }

/// Additive selection evidence in a subject's existing S0 provider projection.
/// It is not admission, a send identity, or authority to alter a sealed request.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FrozenQuotaSelection {
    pub policy_id: EntityId,
    pub policy_revision: u64,
    pub policy_digest: shenko_types::object::Digest,
    pub decision_id: EntityId,
    pub profile_id: String,
    /// Absence preserves the implicit effort of historical subject seals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<FrozenQuotaEffort>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FrozenQuotaEffort {
    pub requested: String,
    pub native: shenko_broker::effort::ProviderEffort,
}

pub(crate) struct QuotaRoute {
    /// None preserves the configured path: no policy selected an override,
    /// the child contract fixed the route, or an active wait already names it.
    pub broker: Option<shenko_broker::broker::BrokerHandle>,
    pub selection: Option<FrozenQuotaSelection>,
}

#[derive(Clone, Copy)]
pub(crate) enum QuotaRouteOwner {
    Subject(EntityId),
    FiniteChild,
}

fn matches_quota_profile(
    binding: &shenko_broker::request::ProviderBinding,
    profile: &shenko_quota::ModelProfile,
) -> bool {
    profile.model_ref == binding.model()
        && profile.local == matches!(binding, shenko_broker::request::ProviderBinding::Local { .. })
}

/// Resolve only installed, currently eligible runtimes. The configured
/// transport wins when it can serve the model; otherwise exactly one installed
/// transport must identify it. A model name cannot choose between accounts.
fn installed_quota_route(
    broker: &shenko_broker::broker::BrokerHandle,
    bindings: &[shenko_broker::request::ProviderBinding],
    profile: &shenko_quota::ModelProfile,
) -> Result<Option<shenko_broker::broker::BrokerHandle>, ShenkoError> {
    use shenko_broker::request::ProviderBinding;
    let compatible = |binding: &ProviderBinding| profile.local == matches!(binding, ProviderBinding::Local { .. });
    if compatible(broker.binding()) && !matches!(broker.binding(), ProviderBinding::Stub { .. }) {
        let selected = if broker.binding().model() == profile.model_ref {
            broker.clone()
        } else {
            broker.for_model(&profile.model_ref)
                .map_err(|error| ShenkoError::conflict(format!("invalid quota model binding: {error:#}")))?
        };
        if selected.serves_model(&profile.model_ref) {
            return Ok(Some(selected));
        }
    }
    let mut selected = None;
    for binding in bindings {
        if !compatible(binding) || matches!(binding, ProviderBinding::Stub { .. }) {
            continue;
        }
        let candidate = broker.for_binding(binding)
            .map_err(|error| ShenkoError::conflict(format!("installed quota route cannot resolve its physical owner: {error:#}")))?;
        let candidate = if binding.model() == profile.model_ref {
            candidate
        } else {
            candidate.for_model(&profile.model_ref)
                .map_err(|error| ShenkoError::conflict(format!("invalid quota model binding: {error:#}")))?
        };
        if !candidate.serves_model(&profile.model_ref) {
            continue;
        }
        if selected.is_some() {
            return Err(ShenkoError::conflict(
                "quota model identifies multiple installed transports; bind the provider/account explicitly",
            ));
        }
        selected = Some(candidate);
    }
    Ok(selected)
}

fn decision_is_current(
    policy: &shenko_quota::QuotaPolicy,
    decision: &shenko_quota::PolicyDecision,
    availability: &std::collections::HashMap<String, shenko_quota::BackendState>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    decision.decided_at <= now && now < decision.valid_until
        && decision.policy_id == policy.policy_id && decision.policy_revision == policy.revision
        && decision.availability.len() == availability.len()
        && decision.availability.iter().all(|observed|
            availability.get(&observed.profile_id) == Some(&observed.state))
}

fn effort_binding_error(code: &str, reason: &str) -> ShenkoError {
    ShenkoError::Protocol(shenko_types::error::ProtocolError::new(
        code, shenko_types::error::ErrorScope::Request,
        shenko_types::error::RetryStrategy::NewRevision,
    ).with_details(serde_json::json!({ "reason": reason })))
}

fn requested_effort(decision: &shenko_quota::PolicyDecision) -> Option<&str> {
    match &decision.choice {
        shenko_quota::Choice::Cloud { effort, .. } => effort.as_deref(),
        _ => None,
    }
}

fn resolve_effort(
    broker: &shenko_broker::broker::BrokerHandle,
    requested: &str,
) -> Result<shenko_broker::effort::ProviderEffort, ShenkoError> {
    broker.resolve_effort(requested).map_err(|error| match error {
        shenko_broker::effort::EffortResolutionError::Unsupported => effort_binding_error(
            "QUOTA_EFFORT_BINDING_UNSUPPORTED",
            "the exact bound runtime has no typed native mapping for the requested effort; no new seal/admission",
        ),
        shenko_broker::effort::EffortResolutionError::ProviderUnavailable => effort_binding_error(
            "MODEL_PROVIDER_BINDING_MISMATCH",
            "the exact runtime owner disappeared during effort selection; no new seal/admission",
        ),
    })
}

fn require_sealed_effort(
    broker: &shenko_broker::broker::BrokerHandle,
    decision: &shenko_quota::PolicyDecision,
    selection: Option<&FrozenQuotaSelection>,
) -> Result<(), ShenkoError> {
    if !matches!(&decision.choice,
        shenko_quota::Choice::Cloud { .. } | shenko_quota::Choice::Local { .. })
    {
        // A quota/capacity wait has not selected priced request semantics.
        return Ok(());
    }
    let sealed = selection.and_then(|selection| selection.effort.as_ref());
    let matches = match (requested_effort(decision), sealed) {
        (None, None) => true,
        (Some(requested), Some(sealed)) => {
            let native = resolve_effort(broker, requested)?;
            requested == sealed.requested && native == sealed.native
        }
        (Some(requested), None) => {
            // Unsupported mappings retain their typed refusal even when a
            // policy appeared after an implicit historical request sealed.
            resolve_effort(broker, requested)?;
            false
        }
        (None, Some(_)) => false,
    };
    if matches { Ok(()) } else {
        Err(effort_binding_error("QUOTA_EFFORT_BINDING_MISMATCH",
            "current priced effort differs from the original sealed native semantics; a new request and seal are required"))
    }
}

/// Select semantics for a fresh subject or child request before sealing it.
/// Quota waits retain the original post-seal boundary for spend admission.
pub(crate) async fn select_quota_route(
    store: &shenko_store::Store,
    broker: Option<&shenko_broker::broker::BrokerHandle>,
    cancel: &tokio_util::sync::CancellationToken,
    strategies: &shenko_quota::QuotaStrategies,
    owner: QuotaRouteOwner,
) -> Result<Option<QuotaRoute>, ShenkoError> {
    loop {
        let Some(policy) = store.latest_quota_policy()? else {
            return Ok(Some(QuotaRoute { broker: None, selection: None }));
        };
        if cancel.is_cancelled() { return Ok(None); }
        let broker = broker.ok_or_else(|| ShenkoError::other(
            "capacity_blocked: quota selection has no installed broker",
        ))?;
        let bindings = match owner {
            QuotaRouteOwner::Subject(_) => Some(broker.installed_bindings()),
            QuotaRouteOwner::FiniteChild => None,
        };
        let mut routes = std::collections::HashMap::new();
        let mut availability = std::collections::HashMap::new();
        for profile in &policy.profiles {
            let usable = if let Some(bindings) = &bindings {
                if let Some(route) = installed_quota_route(broker, bindings, profile)? {
                    routes.insert(profile.profile_id.clone(), route);
                    true
                } else {
                    false
                }
            } else {
                matches_quota_profile(broker.binding(), profile) && broker.serves_model(&profile.model_ref)
            };
            availability.insert(profile.profile_id.clone(), if usable {
                shenko_quota::BackendState::Available
            } else {
                shenko_quota::BackendState::Unavailable
            });
        }
        let now = chrono::Utc::now();
        let latest = store.latest_quota_decision()?;
        let decision = match &latest {
            Some((_, decision)) if decision_is_current(&policy, decision, &availability, now) => decision.clone(),
            _ => {
                let work = match owner {
                    QuotaRouteOwner::Subject(agent) => store.quota_work_evidence(agent)?,
                    QuotaRouteOwner::FiniteChild => None,
                };
                compute_quota_decision(
                    store, &policy, &availability, latest.as_ref().map(|(_, decision)| decision),
                    work.as_ref(), now, strategies,
                )?
            }
        };
        let until = match &decision.choice {
            shenko_quota::Choice::Cloud { profile_id, .. }
            | shenko_quota::Choice::Local { profile_id } => {
                if availability.get(profile_id) != Some(&shenko_quota::BackendState::Available) {
                    return Err(ShenkoError::conflict("quota strategy selected an ineligible installed route"));
                }
                let selected = routes.remove(profile_id);
                let effort = match requested_effort(&decision) {
                    Some(requested) => {
                        let native = resolve_effort(selected.as_ref().unwrap_or(broker), requested)?;
                        Some(FrozenQuotaEffort { requested: requested.to_owned(), native })
                    }
                    None => None,
                };
                return Ok(Some(QuotaRoute {
                    broker: selected,
                    selection: Some(FrozenQuotaSelection {
                        policy_id: policy.policy_id,
                        policy_revision: policy.revision,
                        policy_digest: decision.policy_digest,
                        decision_id: decision.decision_id,
                        profile_id: profile_id.clone(),
                        effort,
                    }),
                }));
            }
            shenko_quota::Choice::Wait { until }
            | shenko_quota::Choice::ProviderExhausted { until, .. } => *until,
            shenko_quota::Choice::ObservationBlocked { .. } => None,
            shenko_quota::Choice::NoBackend => return Err(ShenkoError::other(
                "capacity_blocked: no quota profile has an available installed runtime",
            )),
            shenko_quota::Choice::CapacityUnknown { .. } => return Err(ShenkoError::other(
                "capacity_unknown: quota strategy could not establish an installed runtime",
            )),
        };
        let wait = until.and_then(|at| (at - chrono::Utc::now()).to_std().ok())
            .unwrap_or_else(|| std::time::Duration::from_secs(30))
            .clamp(std::time::Duration::from_millis(50), std::time::Duration::from_secs(30));
        tokio::select! {
            _ = cancel.cancelled() => return Ok(None),
            _ = tokio::time::sleep(wait) => {},
        }
    }
}

fn compute_quota_decision(
    store: &shenko_store::Store,
    policy: &shenko_quota::QuotaPolicy,
    availability: &std::collections::HashMap<String, shenko_quota::BackendState>,
    previous: Option<&shenko_quota::PolicyDecision>,
    work: Option<&shenko_quota::WorkEvidence>,
    now: chrono::DateTime<chrono::Utc>,
    strategies: &shenko_quota::QuotaStrategies,
) -> Result<shenko_quota::PolicyDecision, ShenkoError> {
    // Forecast each (provider, account scope) the policy's cloud
    // profiles touch, only from the latest recorded snapshot of
    // a source the policy actually admits, computed under that
    // source's own TTL/stale-use options.
    let mut forecasts = Vec::new();
    let mut scopes = std::collections::BTreeSet::new();
    for profile in &policy.profiles {
        if profile.local {
            continue;
        }
        for cost in &profile.cost_per_request {
            scopes.insert((
                cost.window.provider_id.clone(),
                cost.window.account_scope_ref.clone(),
            ));
        }
    }
    for (provider_id, scope) in scopes {
        for source in &policy.observation_sources {
            // A policy that pins a source contract forecasts from
            // exactly that contract's reading. Two contracts of one
            // source kind are different facts: a configured limit
            // document and a usage report never stand in for each
            // other.
            let snapshot = match &source.contract {
                Some(contract) => store.latest_quota_snapshot_for_contract(
                    &provider_id,
                    &scope,
                    source.source_kind,
                    contract,
                )?,
                None => store
                    .latest_quota_snapshot_for_source(&provider_id, &scope, source.source_kind)?,
            };
            let Some(snapshot) = snapshot else {
                continue;
            };
            let options = source.options();
            let attempts = store.quota_attempts(&provider_id, &scope)?;
            let assumptions: std::collections::HashMap<
                String,
                shenko_quota::WindowAssumptions,
            > = policy
                .assumptions
                .iter()
                .filter(|entry| {
                    entry.window.provider_id == provider_id
                        && entry.window.account_scope_ref == scope
                })
                .map(|entry| (entry.window.window_key.clone(), entry.value))
                .collect();
            // The forecast algorithm is a replaceable strategy
            // Its refusal is a reconciliation fact,
            // never a guessed or empty forecast: nothing about the
            // account is assumed when the strategy declines.
            let forecast = strategies
                .forecast(shenko_quota::ForecastRequest {
                    snapshot: &snapshot,
                    attempts: &attempts,
                    assumptions: &assumptions,
                    now,
                    options,
                })
                .map_err(|error| {
                    ShenkoError::conflict(&format!(
                        "quota forecast strategy refused: {error}"
                    ))
                })?;
            store.record_quota_forecast(&forecast)?;
            forecasts.push(forecast);
        }
    }
    // The router is a replaceable strategy too; the recorded
    // decision it produces is still validated against the recorded
    // policy and forecasts by the durable admission path.
    let decision = strategies
        .route(shenko_quota::RouteRequest {
            policy,
            forecasts: &forecasts,
            availability,
            previous,
            work,
            now,
        })
        .map_err(|error| {
            ShenkoError::conflict(&format!("quota policy cannot decide: {error}"))
        })?;
    store.record_quota_decision(&decision)?;
    Ok(decision)
}

pub(crate) async fn quota_dispatch_gate(store: &shenko_store::store::Store,
broker: &shenko_broker::broker::BrokerHandle,
request_id: EntityId,
cancel: &tokio_util::sync::CancellationToken,
strategies: &shenko_quota::QuotaStrategies,
native: Option<&shenko_types::task::TaskInvocation>,
selection: Option<&FrozenQuotaSelection>,) -> Result<QuotaGate, ShenkoError> {
    quota_dispatch_gate_for_owner(store, broker, request_id, cancel, strategies,
        QuotaOwner { native, subject_agent: None, selection }).await
}

pub(crate) async fn subject_quota_dispatch_gate(
    store: &shenko_store::Store,
    broker: &shenko_broker::broker::BrokerHandle,
    request_id: EntityId,
    cancel: &tokio_util::sync::CancellationToken,
    strategies: &shenko_quota::QuotaStrategies,
    agent: EntityId,
    selection: Option<&FrozenQuotaSelection>,
) -> Result<QuotaGate, ShenkoError> {
    quota_dispatch_gate_for_owner(store, broker, request_id, cancel, strategies,
        QuotaOwner { native: None, subject_agent: Some(agent), selection }).await
}

struct QuotaOwner<'a> {
    native: Option<&'a shenko_types::task::TaskInvocation>,
    subject_agent: Option<EntityId>,
    selection: Option<&'a FrozenQuotaSelection>,
}

async fn quota_dispatch_gate_for_owner(
    store: &shenko_store::Store,
    broker: &shenko_broker::broker::BrokerHandle,
    request_id: EntityId,
    cancel: &tokio_util::sync::CancellationToken,
    strategies: &shenko_quota::QuotaStrategies,
    owner: QuotaOwner<'_>,
) -> Result<QuotaGate, ShenkoError> {
use shenko_quota::{AdmissionOutcome, AdmissionState, DispatchEvidence};
let QuotaOwner { native, subject_agent, selection } = owner;

let binding = broker.binding();
if store.latest_quota_policy()?.is_none() {
    return Ok(QuotaGate::Ready(None));
}
// Measured useful work of this subject's admitted rounds (
// `expected_useful_work`), read from the ledger's own record. Only a caller
// that IS the subject passes its identity: a finite model child or an
// intervention worker spends rounds in a different unit, so its expectation
// stays unknown rather than borrowing the subject's measurement.
let work = match subject_agent {
    Some(agent_id) => store.quota_work_evidence(agent_id)?,
    None => None,
};
// Admission identity is one wire SEND of the immutable sealed request.
// The gate offers exactly the send the CAS will claim — the LATEST
// unclaimed send with a live reservation — so selection and claim can
// never diverge: a claimed send is durably bound, a cancelled one is
// dead, and only when none is claimable does a retry mint a fresh
// send_id with its own budget and pacing. Unclaimed sends are reused
// across gate re-entries instead of minting ghost budgets.
let pacer = store.quota_pacer()?;
let sends = store.quota_send_ids(request_id)?;
let send_id = sends
    .iter()
    .rev()
    .find(|(send_id, claim_seq)| {
        *claim_seq == 0
            && pacer.reservation(send_id).is_some_and(|reservation| {
                reservation.admission.state != AdmissionState::Cancelled
            })
    })
    .map(|(send_id, _)| *send_id)
    .unwrap_or_else(EntityId::new);
// Recomputations and re-eligible outcomes that make no progress are
// bounded; a decision that still cannot settle is a reconciliation
// fact, not a spin. The counter covers only CONSECUTIVE recompute
// rounds: a genuine quota wait resets it, so a long-lived wait can
// never run the runner out of budget and kill the subject.
let mut recompute_rounds = 0_u32;
loop {
    if cancel.is_cancelled() || native.map(|ctx| store.native_invocation_current(ctx))
        .transpose().map_err(|error| ShenkoError::other(error.to_string()))? == Some(false)
    {
        return Ok(QuotaGate::Cancelled);
    }
    // Re-read policy after each bounded wait. Saving a replacement policy
    // must release a request that has not dispatched yet; retaining the
    // function-entry snapshot would leave that request blocked forever.
    // Its provider binding and send identity remain frozen, and the pacer
    // retains any already-armed accounting under the original evidence.
    let Some(policy) = store.latest_quota_policy()? else {
        return Ok(QuotaGate::Ready(None));
    };
    // Read-only refresh of this SAME selected send: another entrant may have
    // admitted/armed it during the bounded wait. Current installation cannot
    // revoke that original history. Never select a replacement send here.
    let reconciling_original = quota_send_is_original(store, send_id)?;
    // Fresh admission needs the exact installed runtime, its declared model,
    // compatible transport identity and runtime availability. The request is
    // already sealed: this gate cannot select another model or transport.
    let availability: std::collections::HashMap<String, shenko_quota::BackendState> = policy
        .profiles
        .iter()
        .map(|profile| {
            let usable = matches_quota_profile(binding, profile)
                && (reconciling_original || selection.is_none_or(|sealed| sealed.profile_id == profile.profile_id))
                && (reconciling_original || broker.serves_model(&profile.model_ref));
            (
                profile.profile_id.clone(),
                if usable {
                    shenko_quota::BackendState::Available
                } else {
                    shenko_quota::BackendState::Unavailable
                },
            )
        })
        .collect();
    let now = chrono::Utc::now();
    let latest = store.latest_quota_decision()?;
    // Eligibility can call into the runtime after our detached reservation
    // read. Another entrant may have admitted and armed this same send in
    // that interval. Reuse its durable admission before recording a new
    // routing decision: re-admission would rewrite the original metadata.
    // A changed policy still takes the normal decision/admission path.
    if let Some((admitted_policy, admitted_decision)) = &latest
        && admitted_policy.policy_id == policy.policy_id
        && admitted_policy.revision == policy.revision
    {
        let pacer = store.quota_pacer()?;
        if let Some(reservation) = pacer.reservation(&send_id)
            && reservation.admission.state == AdmissionState::Admitted
            && reservation.admission.decision_id == admitted_decision.decision_id
        {
            let profile = admitted_policy.profiles.iter().find(|profile|
                Some(&profile.profile_id) == reservation.admission.profile_id.as_ref())
                .ok_or_else(|| ShenkoError::conflict("admitted profile is not configured"))?;
            if profile.model_ref != binding.model()
                || profile.local != matches!(binding,
                    shenko_broker::request::ProviderBinding::Local { .. })
            {
                return Err(ShenkoError::conflict(
                    "original quota admission differs from the sealed backend",
                ));
            }
            // Replay arming authenticates the request lineage and
            // every original armed attempt in one Store transaction.
            // An admitted send not yet armed follows the same owner.
            store.arm_quota_send(request_id, &reservation.admission)?;
            return Ok(QuotaGate::Ready(Some(send_id)));
        }
    }
    let (policy_in_use, decision) = match &latest {
        Some((policy_in_use, decision))
            if decision.decided_at <= now
                && now < decision.valid_until
                && decision.policy_id == policy.policy_id
                && decision.policy_revision == policy.revision
                && decision.availability.len() == availability.len()
                && decision.availability.iter().all(|observed|
                    availability.get(&observed.profile_id) == Some(&observed.state)) =>
        {
            (policy_in_use.clone(), decision.clone())
        }
        _ => {
            recompute_rounds += 1;
            if recompute_rounds > 32 {
                return Err(ShenkoError::conflict(
                    "quota decision still requires recompute; reconcile snapshot evidence",
                ));
            }
            let decision = compute_quota_decision(
                store, &policy, &availability, latest.as_ref().map(|(_, decision)| decision),
                work.as_ref(), now, strategies,
            )?;
            (policy.clone(), decision)
        }
    };
    // A replacement decision can withhold admission, but it cannot charge a
    // fresh request at effort absent from its sealed native payload.
    // Original admitted/armed history keeps its existing reconciliation path.
    if !reconciling_original
        && let Err(error) = require_sealed_effort(broker, &decision, selection)
    {
        // A second entrant can promote this exact send while routing is
        // reading detached evidence. Its original admission must be
        // reconciled before a new semantic refusal is returned.
        if quota_send_is_original(store, send_id)? { continue; }
        return Err(error);
    }
    let receipt = store.admit_quota_send_with(
        strategies,
        request_id,
        &policy_in_use,
        &decision,
        send_id,
        &availability,
        chrono::Utc::now(),
    )?;
    match receipt.outcome {
        AdmissionOutcome::Quota(admission) | AdmissionOutcome::Existing { admission }
            if admission.state == AdmissionState::Admitted =>
        {
            let profile = policy_in_use
                .profiles
                .iter()
                .find(|profile| Some(&profile.profile_id) == admission.profile_id.as_ref())
                .ok_or_else(|| ShenkoError::conflict("admitted profile is not configured"))?;
            if profile.model_ref != binding.model() {
                return Err(ShenkoError::Protocol(
                    shenko_types::error::ProtocolError::new(
                        "MODEL_PROVIDER_BINDING_MISMATCH",
                        shenko_types::error::ErrorScope::Request,
                        shenko_types::error::RetryStrategy::NewRevision,
                    )
                    .with_details(serde_json::json!({
                        "reason": "quota decision selected a model outside this seal's backend"
                    })),
                ));
            }
            let pacer = store.quota_pacer()?;
            let Some(reservation) = pacer.reservation(&send_id) else {
                return Err(ShenkoError::conflict("admitted quota reservation vanished"));
            };
            match reservation.dispatch {
                Some(DispatchEvidence::ArmedOrUnknown) => {
                    return Ok(QuotaGate::Ready(Some(send_id)));
                }
                Some(DispatchEvidence::NeverDispatched) | None => {
                    store.arm_quota_send(request_id, &reservation.admission)?;
                    return Ok(QuotaGate::Ready(Some(send_id)));
                }
            }
        }
        AdmissionOutcome::Quota(admission) | AdmissionOutcome::Existing { admission } => {
            use shenko_quota::AdmissionState::*;
            match admission.state {
                Eligible => {
                    recompute_rounds += 1;
                    if recompute_rounds > 32 {
                        return Err(ShenkoError::conflict(
                            "quota admission cannot settle; reconcile evidence",
                        ));
                    }
                    continue;
                }
                QuotaPolicyWait | ProviderExhausted | ObservationBlocked => {
                    // `until=None` states (ObservationBlocked, a decision
                    // selecting no backend) sleep a bounded 30s and
                    // re-evaluate; recorded evidence arriving meanwhile
                    // changes the next decision.
                    let wait = admission
                        .next_eligible_at
                        .and_then(|at| (at - chrono::Utc::now()).to_std().ok())
                        .unwrap_or_else(|| std::time::Duration::from_secs(30))
                        .clamp(
                            std::time::Duration::from_millis(50),
                            std::time::Duration::from_secs(30),
                        );
                    tokio::select! {
                        _ = cancel.cancelled() => return Ok(QuotaGate::Cancelled),
                        _ = tokio::time::sleep(wait) => {}
                    }
                    // A real wait made progress of its own kind: the
                    // immediate-recompute budget is not a lifetime cap.
                    recompute_rounds = 0;
                    continue;
                }
                Cancelled => {
                    return Err(ShenkoError::conflict(
                        "quota reservation is terminal for this request; a changed request needs a new request_id",
                    ));
                }
                Admitted => unreachable!("admitted handled above"),
            }
        }
        AdmissionOutcome::CapacityBlocked { reason } => {
            // Durable admission may observe a promotion after our detached
            // snapshot. Recompute only for that same send's new original
            // history; a fresh refusal or an already-reconciled refusal is
            // still terminal. No send, charge or dispatch authority is minted.
            if !reconciling_original && quota_send_is_original(store, send_id)? {
                continue;
            }
            return Err(ShenkoError::other(format!(
                "capacity_blocked: {reason}"
            )));
        }
        AdmissionOutcome::CapacityUnknown { reason } => {
            return Err(ShenkoError::other(format!(
                "capacity_unknown: {reason}"
            )));
        }
        AdmissionOutcome::Invalid { reason } => {
            return Err(ShenkoError::conflict(format!(
                "quota admission invalid: {reason}"
            )));
        }
    }
} }

/// Classification of this selected send only, including a promotion that
/// happened while routing/admission used a detached snapshot.
fn quota_send_is_original(store: &shenko_store::store::Store, send_id: EntityId)
    -> Result<bool, ShenkoError>
{
    use shenko_quota::{AdmissionState, DispatchEvidence};
    Ok(store.quota_pacer()?.reservation(&send_id).is_some_and(|reservation|
        reservation.admission.state == AdmissionState::Admitted
            || reservation.dispatch == Some(DispatchEvidence::ArmedOrUnknown)))
}

/// The armed reservation of the exact wire send `send_id` this step
/// dispatched, verified against durable lineage: the send must be bound to
/// this request and durably claimed. A result for a request with no such
/// binding keeps quota evidence Unknown rather than guessing.
fn armed_quota_reservation(
    store: &shenko_store::store::Store,
    request_id: EntityId,
    send_id: EntityId,
) -> Option<shenko_quota::Reservation> {
    let pacer = store.quota_pacer().ok()?;
    let bound = store
        .quota_send_ids(request_id)
        .ok()?
        .iter()
        .any(|(known, claim_seq)| *known == send_id && *claim_seq > 0);
    if !bound {
        return None;
    }
    pacer
        .reservation(&send_id)
        .filter(|reservation| {
            reservation.dispatch == Some(shenko_quota::DispatchEvidence::ArmedOrUnknown)
        })
        .cloned()
}

/// Update durable quota observations from one real provider result. This is
/// ordinary-traffic evidence only — never a paid probe — and failure to
/// record it must not fabricate or block the sealed model result.
pub(crate) fn observe_quota_response(store: &shenko_store::store::Store,
binding: &shenko_broker::request::ProviderBinding,
result: &shenko_broker::request::InferenceResult,
request_id: EntityId,
send_id: EntityId,) { use shenko_broker::request::InferenceStatus;

let Some(reservation) = armed_quota_reservation(store, request_id, send_id) else {
    return;
};
// The bound attempt rows carry the scope this reservation actually
// charged. Evidence for a reservation spanning several scopes stays
// unrecorded rather than attributed to a guessed account.
let mut scopes = std::collections::BTreeSet::new();
for bound in &reservation.attempts {
    scopes.insert((
        bound.window.provider_id.clone(),
        bound.window.account_scope_ref.clone(),
    ));
}
let Some((provider_id, scope)) = (match scopes.len() {
    1 => scopes.into_iter().next(),
    _ => None,
}) else {
    return;
};
let context = shenko_quota::ObservationContext {
    provider_id,
    account_scope_ref: scope,
    source_version: binding.api_version().map(str::to_owned),
    attribution: shenko_quota::Attribution::Unknown,
    observed_at: chrono::Utc::now(),
};
let snapshot = if !result.quota_headers.is_empty() {
    shenko_quota::from_api_headers(
        &context,
        result
            .quota_headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
    )
} else if matches!(result.status, InferenceStatus::Rejected { http_status: 429 }) {
    // A rate-limited read is a failed observation, never a balance fact.
    shenko_quota::observation_failed(
        &context,
        shenko_quota::SourceKind::ApiHeaders,
        &[],
        shenko_quota::ObservationError::RateLimited,
    )
} else {
    return;
};
if let Err(error) = store.record_quota_snapshot(&snapshot) {
    warn!(%error, "quota snapshot rejected; response evidence not recorded");
} }

/// A terminal result settles what the provider actually reported for the
/// request's bound attempt rows. Windows that cannot be measured keep
/// billing unknown — a partially measured token cost is never settled to a
/// guessed total.
pub(crate) fn settle_quota_terminal(store: &shenko_store::store::Store,
result: &shenko_broker::request::InferenceResult,
request_id: EntityId,
send_id: EntityId,) {
if result.termination.kind != shenko_broker::request::TerminationKind::ProviderTerminal {
    return;
}
let Some(reservation) = armed_quota_reservation(store, request_id, send_id) else {
    return;
};
let usage = result
    .response
    .as_ref()
    .and_then(|response| response.usage.as_ref());
for bound in &reservation.attempts {
    let measured: Option<u64> = match bound.window.window_key.as_str() {
        "api:requests" => Some(1),
        "api:input-tokens" => usage.and_then(|u| u.input_tokens),
        "api:output-tokens" => usage.and_then(|u| u.output_tokens),
        "api:tokens" => usage.and_then(|u| {
            u.input_tokens.zip(u.output_tokens).map(|(input, output)| {
                input.saturating_add(output)
            })
        }),
        _ => None,
    };
    let Some(measured) = measured else { continue };
    let Some(cost) = shenko_quota::Range::point(measured as f64) else {
        continue;
    };
    if let Err(error) = store.settle_quota_attempt(
        reservation.admission.request_id,
        &bound.window,
        bound.ledger_commit.seq,
        cost,
        shenko_quota::AttemptBilling::Settled,
    ) {
        warn!(%error, "quota attempt settle rejected");
    }
} }

#[cfg(test)]
mod policy_change_tests {
    use super::*;
    use anyhow::Context;
    use shenko_quota::*;
    use std::{sync::Arc, time::Duration};

    type AvailabilityReadHook = Box<dyn FnOnce() -> anyhow::Result<()> + Send>;

    // Availability belongs to the installed runtime. All wire methods retain
    // the real provider implementation; this scenario never dispatches them.
    struct AvailabilityRuntime {
        provider: Arc<shenko_broker::provider::AnthropicProvider>,
        available: std::sync::atomic::AtomicBool,
        // One synchronous independent Store transaction at the exact runtime
        // read, after the gate has detached its selected-send snapshot.
        on_read: parking_lot::Mutex<Option<AvailabilityReadHook>>,
        on_read_result: parking_lot::Mutex<Option<anyhow::Result<()>>>,
    }

    #[async_trait::async_trait]
    impl shenko_broker::registry::ProviderRuntime for AvailabilityRuntime {
        fn profile(&self) -> &shenko_broker::profile::ProviderProfile { self.provider.profile() }
        fn binding(&self) -> shenko_broker::request::ProviderBinding { self.provider.binding() }
        fn accepts_model(&self, model: &str) -> bool {
            shenko_broker::registry::ProviderRuntime::accepts_model(self.provider.as_ref(), model)
        }
        fn available(&self) -> bool {
            let on_read = self.on_read.lock().take();
            if let Some(on_read) = on_read {
                let result = on_read();
                *self.on_read_result.lock() = Some(result);
            }
            self.available.load(std::sync::atomic::Ordering::SeqCst)
        }
        async fn complete(&self, request: &shenko_broker::request::InferenceRequest)
            -> anyhow::Result<shenko_broker::request::InferenceResponse> {
            shenko_broker::registry::ProviderRuntime::complete(self.provider.as_ref(), request).await
        }
        async fn stream(&self, request: &shenko_broker::request::InferenceRequest,
            publisher: &shenko_broker::stream::BoundedStreamPublisher)
            -> anyhow::Result<shenko_broker::stream::StreamOutcome> {
            shenko_broker::registry::ProviderRuntime::stream(self.provider.as_ref(), request, publisher).await
        }
        async fn count_tokens(&self, request: &shenko_broker::request::InferenceRequest)
            -> anyhow::Result<shenko_broker::registry::TokenCount> {
            shenko_broker::registry::ProviderRuntime::count_tokens(self.provider.as_ref(), request).await
        }
        async fn probe(&self) -> anyhow::Result<shenko_broker::capability::BackendCapabilitySnapshot> {
            shenko_broker::registry::ProviderRuntime::probe(self.provider.as_ref()).await
        }
    }

    /// A saved resource-policy change must affect a send still waiting for
    /// admission, without resealing its provider or minting a second send.
    #[test]
    fn waiting_admission_observes_replacement_policy() -> anyhow::Result<()> {
        // Build explicitly so runtime setup errors follow the test's Result
        // instead of the async test macro introducing an implicit expect.
        tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let root = std::env::temp_dir().join(format!("shenko-quota-wait-{}", EntityId::new()));
        std::fs::create_dir_all(&root)?;
        let store = Arc::new(shenko_store::Store::open(
            root.join("shenko.db"),
            root.join("cas"),
        )?);
        let mut policy = QuotaPolicy {
            policy_id: EntityId::new(),
            revision: 1,
            profiles: vec![ModelProfile {
                profile_id: "cloud".into(),
                model_ref: "bound-model".into(),
                local: false,
                cost_per_request: vec![WindowCost {
                    window: WindowRef {
                        provider_id: "synthetic".into(),
                        account_scope_ref: "policy-wait-fixture".into(),
                        window_key: "requests".into(),
                    },
                    unit: "requests".into(),
                    amount: Range::point(1.0).context("fixture cost must be finite")?,
                }],
                effort_levels: Vec::new(),
            }],
            assumptions: Vec::new(),
            min_progress_per_hour: 0.0,
            switching_hysteresis: 0.0,
            on_unknown: UnknownQuota::Block,
            allow_overage: false,
            spending_authority_ref: None,
            decision_ttl: chrono::Duration::minutes(5),
            observation_sources: Vec::new(),
            objective: PolicyObjective::Balanced,
            target_utilization_range: None,
            planning_horizon: None,
            emergency_reserve_by_window: Vec::new(),
            uncertainty_margin: None,
            admission_pacing: AdmissionPacing::default(),
        };
        store.record_quota_policy(&policy, chrono::Utc::now())?;
        // Exercise the actual live eligibility read and its quota caller.
        // These are installation facts; no network/model request is sent.
        let provider = Arc::new(shenko_broker::provider::AnthropicProvider::with_endpoint(
            "synthetic-not-a-credential".into(),
            "http://127.0.0.1:1".into(),
            "bound-model",
        ));
        let registry = Arc::new(shenko_broker::registry::ProviderRegistry::new());
        let epoch = registry.install(provider.clone());
        let domain = shenko_broker::resource::ResourceDomain::new(
            shenko_broker::resource::ResourceDomainIdentity {
                domain_id: provider.profile().resource_domain_id.clone(),
                engine_identity: "quota-eligibility-fixture".into(),
                engine_incarnation: None,
                epoch: 1,
            },
            1,
            shenko_broker::admission::CapacityProvenance::Unknown,
            None,
            shenko_broker::admission::CapacityProvenance::Unknown,
        );
        let (_runner, broker) = shenko_broker::broker::Broker::with_registry(
            Arc::clone(&registry), domain,
        )?;
        let (_offline_runner, offline) = shenko_broker::broker::Broker::offline()?;
        anyhow::ensure!(!offline.serves_model("stub"));
        anyhow::ensure!(broker.serves_model("bound-model"));
        // Anthropic sends the sealed caller-selected model, even when it
        // differs from this transport's configured default.
        let selected = broker.for_model("caller-selected-model")?;
        anyhow::ensure!(selected.serves_model("caller-selected-model"));
        anyhow::ensure!(broker.for_binding(selected.binding()).is_ok());
        // The actual fixed-model local runtime retains strict fresh
        // eligibility; transport-compatible recovery remains available.
        let (_fixed_runner, fixed) = shenko_broker::broker::Broker::with_local_provider(
            shenko_broker::local_provider::LocalModelProvider::new(
                "http://127.0.0.1:1".into(), "fixed-model".into(),
            ), 1,
        )?;
        anyhow::ensure!(fixed.serves_model("fixed-model"));
        let undeclared = fixed.for_model("undeclared-model")?;
        anyhow::ensure!(!undeclared.serves_model("undeclared-model"));
        anyhow::ensure!(fixed.for_binding(undeclared.binding()).is_ok());
        // Exercise the borrowed Profile path as well as the legacy binding.
        let profile_broker = broker.for_binding(&provider.profile().binding())?;
        anyhow::ensure!(profile_broker.serves_model("bound-model"));
        let selected_profile = profile_broker.for_model("caller-selected-model")?;
        anyhow::ensure!(selected_profile.serves_model("caller-selected-model"));
        anyhow::ensure!(registry.uninstall(&provider.profile().profile_id, epoch));
        anyhow::ensure!(!broker.serves_model("bound-model"));
        anyhow::ensure!(!profile_broker.serves_model("bound-model"));
        anyhow::ensure!(!selected.serves_model("caller-selected-model"));
        anyhow::ensure!(!selected_profile.serves_model("caller-selected-model"));
        let unavailable = Arc::new(AvailabilityRuntime {
            provider: Arc::clone(&provider),
            available: std::sync::atomic::AtomicBool::new(false),
            on_read: parking_lot::Mutex::new(None),
            on_read_result: parking_lot::Mutex::new(None),
        });
        registry.install(unavailable.clone());
        anyhow::ensure!(!matches!(broker.binding(), shenko_broker::request::ProviderBinding::Stub { .. }));
        anyhow::ensure!(!broker.serves_model("bound-model"));
        anyhow::ensure!(!profile_broker.serves_model("bound-model"));
        let refused_request = EntityId::new();
        anyhow::ensure!(!selected.serves_model("caller-selected-model"));
        anyhow::ensure!(!selected_profile.serves_model("caller-selected-model"));
        let before_refusal = serde_json::to_value(store.quota_pacer()?)?;
        let refusal = tokio::time::timeout(Duration::from_secs(5), quota_dispatch_gate(
            &store, &broker, refused_request, &tokio_util::sync::CancellationToken::new(),
            &QuotaStrategies::baseline(), None, None,
        )).await?;
        let Err(refusal) = refusal else { anyhow::bail!("fresh unavailable runtime must refuse admission"); };
        anyhow::ensure!(refusal.to_string().contains("capacity_blocked"), "unexpected refusal: {refusal}");
        anyhow::ensure!(store.quota_send_ids(refused_request)?.is_empty());
        anyhow::ensure!(serde_json::to_value(store.quota_pacer()?)? == before_refusal);
        anyhow::ensure!(store.quota_attempts("synthetic", "policy-wait-fixture")?.is_empty());
        // Only runtime availability changes; identity remains exactly equal.
        unavailable.available.store(true, std::sync::atomic::Ordering::SeqCst);
        anyhow::ensure!(broker.serves_model("bound-model"));
        anyhow::ensure!(profile_broker.serves_model("bound-model"));
        anyhow::ensure!(selected.serves_model("caller-selected-model"));
        anyhow::ensure!(selected_profile.serves_model("caller-selected-model"));
        // The same profile id/model at a different endpoint is not eligible
        // for this already selected handle's transport identity.
        registry.install(Arc::new(shenko_broker::provider::AnthropicProvider::with_endpoint(
            "synthetic-not-a-credential".into(),
            "http://127.0.0.1:2".into(),
            "bound-model",
        )));
        anyhow::ensure!(!broker.serves_model("bound-model"));
        anyhow::ensure!(!profile_broker.serves_model("bound-model"));
        anyhow::ensure!(!selected.serves_model("caller-selected-model"));
        anyhow::ensure!(!selected_profile.serves_model("caller-selected-model"));
        let restored_epoch = registry.install(provider);
        anyhow::ensure!(broker.serves_model("bound-model"));
        anyhow::ensure!(profile_broker.serves_model("bound-model"));
        let request_id = EntityId::new();
        let cancel = tokio_util::sync::CancellationToken::new();
        let run_store = Arc::clone(&store);
        let run_cancel = cancel.clone();
        let original_broker = broker.clone();
        let run = tokio::spawn(async move {
            quota_dispatch_gate(
                &run_store,
                &broker,
                request_id,
                &run_cancel,
                &QuotaStrategies::baseline(),
                None,
                None,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while store.quota_send_ids(request_id)?.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<(), anyhow::Error>(())
        })
        .await??;
        let waiting = store.quota_send_ids(request_id)?;
        anyhow::ensure!(waiting.len() == 1 && waiting[0].1 == 0);
        policy.revision = 2;
        policy.on_unknown = UnknownQuota::Admit;
        store.record_quota_policy(&policy, chrono::Utc::now())?;
        // A second entrant admits and arms the SAME waiting send. The first
        // entrant must refresh its original-history classification on wake.
        let promoted = quota_dispatch_gate(
            &store, &original_broker, request_id,
            &tokio_util::sync::CancellationToken::new(),
            &QuotaStrategies::baseline(), None, None,
        ).await?;
        anyhow::ensure!(matches!(promoted, QuotaGate::Ready(Some(send)) if send == waiting[0].0));
        let original_reservation = serde_json::to_value(store.quota_pacer()?.reservation(&waiting[0].0))?;
        let original_attempts = serde_json::to_value(store.quota_attempts("synthetic", "policy-wait-fixture")?)?;
        anyhow::ensure!(registry.uninstall(original_broker.binding().profile_id(), restored_epoch));
        anyhow::ensure!(!original_broker.serves_model("bound-model"));
        // Uses the production bounded wait; the test does not change policy
        // timings or substitute a decision/dispatcher implementation.
        let outcome = tokio::time::timeout(Duration::from_secs(35), run).await;
        cancel.cancel();
        let result = outcome???;
        anyhow::ensure!(matches!(result, QuotaGate::Ready(Some(send)) if send == waiting[0].0));
        // An armed original keeps its exact reservation after uninstall.
        // Fresh eligibility is false, but it cannot gate reconciliation or
        // manufacture another send/charge for the original request.
        anyhow::ensure!(!original_broker.serves_model("bound-model"));
        let reconciled = quota_dispatch_gate(
            &store, &original_broker, request_id,
            &tokio_util::sync::CancellationToken::new(),
            &QuotaStrategies::baseline(), None, None,
        ).await?;
        anyhow::ensure!(matches!(reconciled, QuotaGate::Ready(Some(send)) if send == waiting[0].0));
        anyhow::ensure!(store.quota_send_ids(request_id)? == waiting);
        anyhow::ensure!(serde_json::to_value(store.quota_pacer()?.reservation(&waiting[0].0))? == original_reservation);
        anyhow::ensure!(serde_json::to_value(store.quota_attempts("synthetic", "policy-wait-fixture")?)? == original_attempts);
        let (_, decision) = store
            .latest_quota_decision()?
            .context("missing quota admission decision")?;
        anyhow::ensure!(decision.policy_revision == 2);
        drop(store);
        std::fs::remove_dir_all(root)?;
        Ok(())
        })
    }
    /// Force promotion after the detached waiting snapshot, while the runtime
    /// read reports unavailable, before durable admission sees NoBackend.
    #[test]
    fn selected_send_promotion_during_eligibility_preserves_original() -> anyhow::Result<()> {
        fn admit_selected(store: &shenko_store::Store, policy: &QuotaPolicy,
            request: EntityId, send: EntityId) -> anyhow::Result<AdmissionOutcome>
        {
            let strategies = QuotaStrategies::baseline();
            let availability = std::collections::HashMap::from([
                ("cloud".to_owned(), BackendState::Available),
            ]);
            let decision = strategies.route(RouteRequest {
                policy, forecasts: &[], availability: &availability,
                previous: None, work: None, now: chrono::Utc::now(),
            })?;
            store.record_quota_decision(&decision)?;
            Ok(store.admit_quota_send_with(&strategies, request, policy,
                &decision, send, &availability, chrono::Utc::now())?.outcome)
        }
        tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let root = std::env::temp_dir().join(format!("shenko-quota-promotion-{}", EntityId::new()));
        std::fs::create_dir_all(&root)?;
        let store = Arc::new(shenko_store::Store::open(root.join("store.db"), root.join("cas"))?);
        let mut policy = QuotaPolicy {
            policy_id: EntityId::new(),
            revision: 1,
            profiles: vec![ModelProfile {
                profile_id: "cloud".into(),
                model_ref: "bound-model".into(),
                local: false,
                cost_per_request: vec![WindowCost {
                    window: WindowRef {
                        provider_id: "synthetic".into(),
                        account_scope_ref: "policy-wait-fixture".into(),
                        window_key: "requests".into(),
                    },
                    unit: "requests".into(),
                    amount: Range::point(1.0).context("fixture cost must be finite")?,
                }],
                effort_levels: Vec::new(),
            }],
            assumptions: Vec::new(),
            min_progress_per_hour: 0.0,
            switching_hysteresis: 0.0,
            on_unknown: UnknownQuota::Block,
            allow_overage: false,
            spending_authority_ref: None,
            decision_ttl: chrono::Duration::minutes(5),
            observation_sources: Vec::new(),
            objective: PolicyObjective::Balanced,
            target_utilization_range: None,
            planning_horizon: None,
            emergency_reserve_by_window: Vec::new(),
            uncertainty_margin: None,
            admission_pacing: AdmissionPacing::default(),
        };
        store.record_quota_policy(&policy, chrono::Utc::now())?;
        let request = EntityId::new();
        let send = EntityId::new();
        let waiting = admit_selected(&store, &policy, request, send)?;
        anyhow::ensure!(matches!(&waiting, AdmissionOutcome::Quota(admission)
            | AdmissionOutcome::Existing { admission }
            if admission.state != AdmissionState::Admitted));
        anyhow::ensure!(!quota_send_is_original(&store, send)?);
        let lineage = store.quota_send_ids(request)?;
        anyhow::ensure!(lineage == vec![(send, 0)]);
        policy.revision = 2;
        policy.on_unknown = UnknownQuota::Admit;
        store.record_quota_policy(&policy, chrono::Utc::now())?;
        let promoted_snapshot = Arc::new(std::sync::Mutex::new(None));
        let hook_snapshot = Arc::clone(&promoted_snapshot);
        let hook_store = Arc::clone(&store);
        let runtime = Arc::new(AvailabilityRuntime {
            provider: Arc::new(shenko_broker::provider::AnthropicProvider::with_endpoint(
                "synthetic-not-a-credential".into(), "http://127.0.0.1:1".into(), "bound-model")),
            available: std::sync::atomic::AtomicBool::new(false),
            on_read: parking_lot::Mutex::new(Some(Box::new(move || {
                // This is a real second durable entrant, not a fabricated
                // receipt. It promotes only the already-selected send.
                let promoted = admit_selected(&hook_store, &policy, request, send)?;
                let admission = match promoted {
                    AdmissionOutcome::Quota(admission) | AdmissionOutcome::Existing { admission }
                        if admission.state == AdmissionState::Admitted => admission,
                    other => anyhow::bail!("independent promotion failed: {other:?}"),
                };
                hook_store.arm_quota_send(request, &admission)?;
                *hook_snapshot.lock().map_err(|_| anyhow::anyhow!("fixture snapshot lock"))? = Some((
                    serde_json::to_value(hook_store.quota_pacer()?.reservation(&send))?,
                    serde_json::to_value(hook_store.quota_attempts("synthetic", "policy-wait-fixture")?)?,
                    serde_json::to_value(hook_store.latest_quota_decision()?)?,
                ));
                Ok(())
            }))),
            on_read_result: parking_lot::Mutex::new(None),
        });
        let registry = Arc::new(shenko_broker::registry::ProviderRegistry::new());
        registry.install(runtime.clone());
        let domain = shenko_broker::resource::ResourceDomain::new(
            shenko_broker::resource::ResourceDomainIdentity {
                domain_id: runtime.provider.profile().resource_domain_id.clone(),
                engine_identity: "promotion-interleaving".into(), engine_incarnation: None, epoch: 1,
            }, 1, shenko_broker::admission::CapacityProvenance::Unknown,
            None, shenko_broker::admission::CapacityProvenance::Unknown,
        );
        let (_runner, broker) = shenko_broker::broker::Broker::with_registry(registry, domain)?;
        let outcome = quota_dispatch_gate(&store, &broker, request,
            &tokio_util::sync::CancellationToken::new(), &QuotaStrategies::baseline(), None, None).await;
        runtime.on_read_result.lock()
            .take().context("the promotion hook must execute inside eligibility")??;
        anyhow::ensure!(matches!(outcome?, QuotaGate::Ready(Some(id)) if id == send));
        let original = promoted_snapshot.lock().map_err(|_| anyhow::anyhow!("fixture snapshot lock"))?
            .take().context("the original armed accounting must be captured")?;
        anyhow::ensure!(serde_json::to_value(store.quota_pacer()?.reservation(&send))? == original.0);
        anyhow::ensure!(serde_json::to_value(store.quota_attempts("synthetic", "policy-wait-fixture")?)? == original.1);
        anyhow::ensure!(serde_json::to_value(store.latest_quota_decision()?)? == original.2);
        anyhow::ensure!(store.quota_send_ids(request)? == lineage);
        anyhow::ensure!(!broker.serves_model("bound-model"));
        drop(store);
        std::fs::remove_dir_all(root)?;
        Ok(())
        })
    }

}
