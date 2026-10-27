//! Quota admission for one runnable request (§42.5). It decides *when* the
//! already-registered next step may spend quota, separately from whether a
//! backend has compute capacity: a capacity block is returned as its own
//! outcome and never recorded as a quota state.
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;

use crate::model::Range;
use crate::policy::{Choice, ModelProfile, PolicyDecision, QuotaPolicy, WindowRef};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionState {
    /// Allowed in principle; needs a current decision before admission.
    Eligible,
    /// Deliberately paced by policy. No compute permit is held and the
    /// provider is not claimed to be exhausted.
    QuotaPolicyWait,
    /// The provider reports the limit reached.
    ProviderExhausted,
    /// Needed quota is unobserved and policy blocks on unknown.
    ObservationBlocked,
    Admitted,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaAdmission {
    pub admission_id: EntityId,
    pub request_id: EntityId,
    pub decision_id: EntityId,
    pub profile_id: Option<String>,
    pub state: AdmissionState,
    pub next_eligible_at: Option<DateTime<Utc>>,
    pub reason: String,
    /// Provisional reservation per window while admitted and unsent.
    pub estimated_cost: Vec<(WindowRef, Range)>,
    pub record_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AdmissionOutcome {
    Quota(QuotaAdmission),
    /// No configured backend can run: report capacity-blocked, not a quota
    /// wait and not "still thinking".
    CapacityBlocked { reason: String },
}

/// Spacing state for paced admissions under one decision's rate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Pacer {
    pub last_admitted_at: Option<DateTime<Utc>>,
}

fn profile<'a>(policy: &'a QuotaPolicy, id: &str) -> Option<&'a ModelProfile> {
    policy.profiles.iter().find(|p| p.profile_id == id)
}

/// Decide admission of `request_id` at `now` under `decision`.
pub fn admit(
    policy: &QuotaPolicy,
    decision: &PolicyDecision,
    request_id: EntityId,
    pacer: &mut Pacer,
    now: DateTime<Utc>,
) -> AdmissionOutcome {
    let record = |state, profile_id: Option<&str>, next: Option<DateTime<Utc>>, reason: &str, cost| {
        AdmissionOutcome::Quota(QuotaAdmission {
            admission_id: EntityId::new(),
            request_id,
            decision_id: decision.decision_id,
            profile_id: profile_id.map(str::to_owned),
            state,
            next_eligible_at: next,
            reason: reason.into(),
            estimated_cost: cost,
            record_revision: 0,
        })
    };
    if decision.policy_id != policy.policy_id || decision.policy_revision != policy.revision || now > decision.valid_until
    {
        return record(AdmissionState::Eligible, None, None, "decision is stale; recompute before admitting", vec![]);
    }
    match &decision.choice {
        Choice::Cloud { profile_id, rate_per_hour, .. } => {
            let Some(p) = profile(policy, profile_id) else {
                return record(AdmissionState::Eligible, None, None, "decided profile no longer configured", vec![]);
            };
            if *rate_per_hour <= 0.0 {
                return record(AdmissionState::QuotaPolicyWait, Some(profile_id), None, "no sustainable rate", vec![]);
            }
            let interval = if rate_per_hour.is_finite() {
                Duration::milliseconds((3_600_000.0 / rate_per_hour).ceil() as i64)
            } else {
                Duration::zero()
            };
            if let Some(next) = pacer.last_admitted_at.map(|t| t + interval).filter(|next| now < *next) {
                return record(AdmissionState::QuotaPolicyWait, Some(profile_id), Some(next), "paced to the sustainable rate", vec![]);
            }
            pacer.last_admitted_at = Some(now);
            let cost = p.cost_per_request.iter().map(|(w, c)| (w.clone(), *c)).collect();
            record(AdmissionState::Admitted, Some(profile_id), None, "within the sustainable rate", cost)
        }
        Choice::Local { profile_id } => {
            record(AdmissionState::Admitted, Some(profile_id), None, "local profile draws no provider quota", vec![])
        }
        Choice::Wait { until } => record(AdmissionState::QuotaPolicyWait, None, *until, "budget returns after reset", vec![]),
        Choice::ProviderExhausted { until, .. } => {
            record(AdmissionState::ProviderExhausted, None, *until, "provider reports the limit reached", vec![])
        }
        Choice::ObservationBlocked { .. } => {
            record(AdmissionState::ObservationBlocked, None, None, "needed quota unobserved; policy blocks", vec![])
        }
        Choice::NoBackend => AdmissionOutcome::CapacityBlocked { reason: "no configured backend can run".into() },
    }
}

/// Cancel an admission. Before the request was sent the reservation is
/// released. After it was sent (armed, possibly billed) the estimate is kept
/// as charges of unknown billing for the caller to record as local attempts;
/// stopping a call does not make it free.
pub fn cancel(admission: &QuotaAdmission, sent: bool) -> (QuotaAdmission, Vec<(WindowRef, Range)>) {
    let mut cancelled = admission.clone();
    cancelled.state = AdmissionState::Cancelled;
    cancelled.record_revision += 1;
    cancelled.estimated_cost = Vec::new();
    let retained = if sent && admission.state == AdmissionState::Admitted {
        admission.estimated_cost.iter().map(|(w, c)| (w.clone(), Range::new(0.0, c.upper))).collect()
    } else {
        Vec::new()
    };
    (cancelled, retained)
}
