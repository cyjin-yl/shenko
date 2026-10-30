//! Shared quota admission and original-send accounting for subject and finite model children.
use shenko_types::{error::ShenkoError, ids::EntityId};
use tracing::warn;

/// Quota admission gate for one sealed request (SPEC §42.5–42.6).
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

pub(crate) async fn quota_dispatch_gate(store: &shenko_store::store::Store,
binding: &shenko_broker::request::ProviderBinding,
request_id: EntityId,
cancel: &tokio_util::sync::CancellationToken,
native: Option<&shenko_store::TaskInvocation>,) -> Result<QuotaGate, ShenkoError> { use shenko_quota::{AdmissionOutcome, AdmissionState, DispatchEvidence};

let Some(policy) = store.latest_quota_policy()? else {
    return Ok(QuotaGate::Ready(None));
};
// A profile is usable only if its declared model runs on the actual
// sealed backend: the bound model name plus the local/cloud split.
let availability: std::collections::HashMap<String, shenko_quota::BackendState> = policy
    .profiles
    .iter()
    .map(|profile| {
        let usable = profile.local
            == matches!(binding, shenko_broker::request::ProviderBinding::Local { .. })
            && profile.model_ref == binding.model();
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
let mut work = 0_u32;
loop {
    if cancel.is_cancelled() || native.map(|ctx| store.native_invocation_current(ctx))
        .transpose().map_err(|error| ShenkoError::other(error.to_string()))? == Some(false)
    {
        return Ok(QuotaGate::Cancelled);
    }
    let now = chrono::Utc::now();
    let latest = store.latest_quota_decision()?;
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
            work += 1;
            if work > 32 {
                return Err(ShenkoError::conflict(
                    "quota decision still requires recompute; reconcile snapshot evidence",
                ));
            }
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
                    let Some(snapshot) = store.latest_quota_snapshot_for_source(
                        &provider_id,
                        &scope,
                        source.source_kind,
                    )? else {
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
                    let forecast = shenko_quota::forecast(
                        &snapshot,
                        &attempts,
                        &assumptions,
                        now,
                        options,
                    );
                    store.record_quota_forecast(&forecast)?;
                    forecasts.push(forecast);
                }
            }
            let previous = latest.as_ref().map(|(_, decision)| decision);
            let decision = shenko_quota::decide(
                &policy,
                &forecasts,
                &availability,
                previous,
                now,
            )
            .map_err(|error| {
                ShenkoError::conflict(&format!("quota policy cannot decide: {error}"))
            })?;
            store.record_quota_decision(&decision)?;
            (policy.clone(), decision)
        }
    };
    let receipt = store.admit_quota_send(
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
                    work += 1;
                    if work > 32 {
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
                    work = 0;
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