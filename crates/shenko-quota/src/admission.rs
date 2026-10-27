//! Quota admission for one runnable request (§42.5). It decides *when* the
//! already-registered next step may spend quota, separately from whether a
//! backend has compute capacity: a capacity block is returned as its own
//! outcome and never recorded as a quota state.
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use shenko_types::commit::CommitRef;
use shenko_types::ids::EntityId;
use std::collections::{BTreeMap, HashMap};

use crate::forecast::{AttemptBilling, LocalAttempt, QuotaForecast};
use crate::model::Range;
use crate::policy::{BackendState, Choice, ModelProfile, PolicyDecision, QuotaPolicy, WindowCost, WindowRef};

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
    /// Actual admission time; waiting records have no admitted time.
    pub admitted_at: Option<DateTime<Utc>>,
    /// Windows whose periodic pacing was advanced by this admission.
    pub paced_windows: Vec<PacingKey>,
    pub reason: String,
    /// Original admitted estimate; outstanding reservations are tracked separately.
    pub estimated_cost: Vec<WindowCost>,
    pub record_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AdmissionOutcome {
    Quota(QuotaAdmission),
    /// The same stable admission is already recorded; do not dispatch again.
    Existing { admission: QuotaAdmission },
    /// No configured backend can run: report capacity-blocked, not a quota
    /// wait and not "still thinking".
    CapacityBlocked { reason: String },
    /// Backend availability has not been observed.
    CapacityUnknown { reason: String },
    /// An inconsistent or malformed decision is not an admission.
    Invalid { reason: String },
}

/// A stable request and its still-outstanding per-window reservation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reservation {
    pub admission: QuotaAdmission,
    pub outstanding: Vec<WindowCost>,
    pub dispatch: Option<DispatchEvidence>,
    pub attempts: Vec<BoundAttempt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundAttempt {
    pub window: WindowRef,
    pub ledger_commit: CommitRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PacingKey {
    pub window: WindowRef,
    pub period_end: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowPace {
    pub key: PacingKey,
    pub request_id: EntityId,
    pub admitted_at: DateTime<Utc>,
}

/// Single-writer spacing and reservation state. The owner must persist it
/// before dispatch and supply complete committed attempts when forecasting.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Pacer {
    pacing: Vec<WindowPace>,
    reservations: BTreeMap<EntityId, Reservation>,
}

impl<'de> Deserialize<'de> for Pacer {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Stored {
            pacing: Vec<WindowPace>,
            reservations: BTreeMap<EntityId, Reservation>,
        }
        let stored = Stored::deserialize(deserializer)?;
        let pacer = Self { pacing: stored.pacing, reservations: stored.reservations };
        if !valid_pacer(&pacer) { return Err(serde::de::Error::custom("inconsistent pacer state")); }
        Ok(pacer)
    }
}

impl Pacer {
    /// Drop only per-window reservations proved covered by this forecast's
    /// bound source snapshot and same-ledger attempt. Identity records remain.
    pub fn reconcile_covered(&mut self, forecast: &QuotaForecast) {
        for reservation in self.reservations.values_mut() {
            let request_id = reservation.admission.request_id;
            let attempts = &reservation.attempts;
            reservation.outstanding.retain(|cost| {
                let window = &cost.window;
                if window.provider_id != forecast.provider_id || window.account_scope_ref != forecast.account_scope_ref {
                    return true;
                }
                let mut found = forecast.windows.iter().filter(|w| w.window_key == window.window_key && w.unit == cost.unit);
                let Some(item) = found.next() else { return true };
                if found.next().is_some() { return true; }
                let Some(bound) = attempts.iter().find(|bound| bound.window == *window) else { return true };
                !item.accounted_attempts.iter().any(|a| a.request_id == request_id && a.covered
                    && a.ledger_commit == bound.ledger_commit)
            });
        }
    }
}

fn profile<'a>(policy: &'a QuotaPolicy, id: &str) -> Option<&'a ModelProfile> {
    policy.profiles.iter().find(|p| p.profile_id == id)
}

fn unavailable(availability: &HashMap<String, BackendState>, profile_id: &str) -> Option<AdmissionOutcome> {
    match availability.get(profile_id).copied().unwrap_or(BackendState::Unknown) {
        BackendState::Available => None,
        BackendState::Unavailable => Some(AdmissionOutcome::CapacityBlocked { reason: "selected backend is unavailable".into() }),
        BackendState::Unknown => Some(AdmissionOutcome::CapacityUnknown { reason: "selected backend availability is unknown".into() }),
    }
}

fn record_admission(pacer: &mut Pacer, admission: QuotaAdmission, outstanding: Vec<WindowCost>) -> AdmissionOutcome {
    if let Some(previous) = pacer.reservations.get(&admission.request_id) {
        let prior = &previous.admission;
        if prior.decision_id == admission.decision_id && prior.profile_id == admission.profile_id
            && prior.state == admission.state && prior.next_eligible_at == admission.next_eligible_at
            && prior.reason == admission.reason && prior.estimated_cost == admission.estimated_cost
            && prior.paced_windows == admission.paced_windows
        {
            return AdmissionOutcome::Existing { admission: prior.clone() };
        }
    }
    pacer.reservations.insert(admission.request_id, Reservation { admission: admission.clone(), outstanding,
        dispatch: None, attempts: Vec::new() });
    AdmissionOutcome::Quota(admission)
}

fn valid_pacer(pacer: &Pacer) -> bool {
    let mut original = std::collections::HashSet::new();
    let mut paced = std::collections::HashSet::new();
    let mut outstanding = std::collections::HashSet::new();
    let mut bound = std::collections::HashSet::new();
    for (request_id, reservation) in &pacer.reservations {
        let admission = &reservation.admission;
        if *request_id != admission.request_id { return false; }
        let admitted = admission.state == AdmissionState::Admitted;
        let cancelled = admission.state == AdmissionState::Cancelled;
        if admitted && admission.admitted_at.is_none() { return false; }
        if cancelled && reservation.dispatch.is_none() { return false; }
        if admitted && reservation.dispatch == Some(DispatchEvidence::NeverDispatched) { return false; }
        if !admitted && !cancelled && reservation.dispatch.is_some() { return false; }
        if reservation.dispatch == Some(DispatchEvidence::NeverDispatched) && !reservation.outstanding.is_empty() { return false; }
        if !admitted && !cancelled && (admission.admitted_at.is_some() || !admission.paced_windows.is_empty() || !reservation.outstanding.is_empty()) { return false; }
        original.clear();
        for cost in &admission.estimated_cost {
            if cost.unit.is_empty() || Range::new(cost.amount.lower, cost.amount.upper).is_none()
                || !original.insert(&cost.window) { return false; }
        }
        bound.clear();
        for attempt in &reservation.attempts {
            if !bound.insert(&attempt.window) || !original.contains(&attempt.window)
                || attempt.ledger_commit.ledger_id == EntityId::nil() || attempt.ledger_commit.seq.0 == 0
            {
                return false;
            }
        }
        if reservation.dispatch == Some(DispatchEvidence::ArmedOrUnknown) {
            if reservation.outstanding.iter().any(|cost| !bound.contains(&cost.window)) { return false; }
        } else if !reservation.attempts.is_empty() {
            return false;
        }
        paced.clear();
        for key in &admission.paced_windows {
            if !paced.insert(key) || !original.contains(&key.window) { return false; }
            if reservation.dispatch != Some(DispatchEvidence::NeverDispatched) {
                let Some(at) = admission.admitted_at else { return false };
                if at >= key.period_end || !pacer.pacing.iter().any(|pace| pace.key == *key && pace.admitted_at >= at) { return false; }
            }
        }
        outstanding.clear();
        for cost in &reservation.outstanding {
            if cost.unit.is_empty() || Range::new(cost.amount.lower, cost.amount.upper).is_none()
                || !outstanding.insert(&cost.window)
                || !admission.estimated_cost.iter().any(|original| original.window == cost.window
                    && original.unit == cost.unit && original.amount.upper >= cost.amount.upper)
            {
                return false;
            }
        }
    }
    let mut windows = std::collections::HashSet::new();
    for pace in &pacer.pacing {
        if !windows.insert(&pace.key) { return false; }
        let Some(reservation) = pacer.reservations.get(&pace.request_id) else { return false };
        if reservation.admission.admitted_at != Some(pace.admitted_at)
            || reservation.dispatch == Some(DispatchEvidence::NeverDispatched)
            || !reservation.admission.paced_windows.contains(&pace.key)
        {
            return false;
        }
    }
    true
}

fn reserved_upper(pacer: &Pacer, constraint: &crate::policy::WindowConstraint) -> Option<f64> {
    let mut total = Range::zero();
    for (request_id, reservation) in &pacer.reservations {
        for cost in &reservation.outstanding {
            if cost.window != constraint.window { continue; }
            if cost.unit != constraint.unit { return None; }
            let amount = Range::new(cost.amount.lower, cost.amount.upper)?;
            let mut accounted = Range::zero();
            let mut covered = false;
            if let Some(bound) = reservation.attempts.iter().find(|bound| bound.window == cost.window) {
                for attempt in constraint.accounted_attempts.iter().filter(|attempt|
                    attempt.request_id == *request_id && attempt.ledger_commit == bound.ledger_commit) {
                    if attempt.covered { covered = true; break; }
                    accounted = accounted.checked_add(Range::point(attempt.cost_upper)?)?;
                }
            }
            if !covered {
                let remainder = Range::point(amount.upper)?.checked_sub(accounted)?;
                total = total.checked_add(remainder)?;
            }
        }
    }
    Some(total.upper)
}

/// Decide admission of `request_id` under a sealed policy decision. The
/// single-writer pacer must be persisted before any external dispatch.
pub fn admit(
    policy: &QuotaPolicy,
    decision: &PolicyDecision,
    request_id: EntityId,
    pacer: &mut Pacer,
    availability: &HashMap<String, BackendState>,
    now: DateTime<Utc>,
) -> AdmissionOutcome {
    let invalid = |reason: &str| AdmissionOutcome::Invalid { reason: reason.into() };
    let (admission_id, revision) = match pacer.reservations.get(&request_id) {
        Some(previous) if matches!(previous.admission.state, AdmissionState::Admitted | AdmissionState::Cancelled) => {
            return if previous.admission.decision_id == decision.decision_id {
                AdmissionOutcome::Existing { admission: previous.admission.clone() }
            } else { invalid("request already has a terminal or admitted record") };
        }
        Some(previous) => {
            let Some(revision) = previous.admission.record_revision.checked_add(1) else { return invalid("admission revision exhausted") };
            (previous.admission.admission_id, revision)
        }
        None => (EntityId::new(), 0),
    };
    let record = |state, profile_id: Option<&str>, next: Option<DateTime<Utc>>, reason: &str, cost| {
        QuotaAdmission {
            admission_id,
            request_id,
            decision_id: decision.decision_id,
            profile_id: profile_id.map(str::to_owned),
            state,
            next_eligible_at: next,
            admitted_at: (state == AdmissionState::Admitted).then_some(now),
            paced_windows: Vec::new(),
            reason: reason.into(),
            estimated_cost: cost,
            record_revision: revision,
        }
    };
    if crate::policy::validate(policy, now).is_err() {
        return invalid("policy is invalid or unsupported");
    }
    if crate::policy::policy_digest(policy).ok().as_ref() != Some(&decision.policy_digest) {
        return invalid("policy content differs from the sealed decision");
    }
    if decision.policy_id != policy.policy_id || decision.policy_revision != policy.revision
        || now < decision.decided_at || now >= decision.valid_until
    {
        return record_admission(pacer, record(AdmissionState::Eligible, None, None,
            "decision is stale or from the future; recompute before admitting", vec![]), Vec::new());
    }
    if decision.availability.len() != policy.profiles.len()
        || decision.availability.iter().zip(&policy.profiles).any(|(observed, profile)| observed.profile_id != profile.profile_id)
    {
        return invalid("decision does not bind every backend observation");
    }
    let mut changed = false;
    let mut any_available = false;
    let mut any_unknown = false;
    for observed in &decision.availability {
        let current = availability.get(&observed.profile_id).copied().unwrap_or(BackendState::Unknown);
        changed |= current != observed.state;
        any_available |= current == BackendState::Available;
        any_unknown |= current == BackendState::Unknown;
    }
    if changed {
        if let Choice::Cloud { profile_id, .. } | Choice::Local { profile_id } = &decision.choice {
            if let Some(outcome) = unavailable(availability, profile_id) { return outcome; }
        }
        if !any_available {
            return if any_unknown { AdmissionOutcome::CapacityUnknown { reason: "backend availability is unknown".into() } }
                else { AdmissionOutcome::CapacityBlocked { reason: "no configured backend can run".into() } };
        }
        return invalid("backend availability changed; recompute the decision");
    }
    match &decision.choice {
        Choice::Cloud { profile_id, rate_per_hour, unbounded_windows } => {
            let Some(p) = profile(policy, profile_id) else {
                return invalid("decided profile no longer configured");
            };
            if p.local { return invalid("cloud choice refers to a local profile"); }
            if !decision.availability.iter().any(|item| item.profile_id == *profile_id && item.state == BackendState::Available) {
                return invalid("selected backend was not available in the decision");
            }
            if let Some(outcome) = unavailable(availability, profile_id) { return outcome; }
            if rate_per_hour.is_some_and(|rate| !rate.is_finite() || rate <= 0.0) {
                return invalid("cloud rate must be finite and positive when present");
            }
            let mut seen = std::collections::HashSet::new();
            let mut attempts = std::collections::HashSet::new();
            let mut period_rate: Option<f64> = None;
            for constraint in &decision.constraints {
                if !seen.insert(&constraint.window) { return invalid("duplicate window constraint"); }
                let Some(cost) = p.cost_per_request.iter().find(|cost| cost.window == constraint.window) else { return invalid("constraint is outside the profile"); };
                attempts.clear();
                if cost.unit != constraint.unit || Range::new(cost.amount.lower, cost.amount.upper).is_none()
                    || cost.amount.upper <= 0.0 || constraint.accounted_attempts.iter().any(|a| !a.cost_upper.is_finite()
                        || a.cost_upper < 0.0 || !attempts.insert((a.request_id, a.ledger_commit)))
                {
                    return invalid("invalid cost, unit, or accounted attempt");
                }
                if constraint.next_change.is_some_and(|at| at <= now) {
                    return record_admission(pacer, record(AdmissionState::Eligible, None, None,
                        "window boundary passed; recompute the decision", vec![]), Vec::new());
                }
                match constraint.available {
                    Some(value) if value.is_finite() && value >= 0.0 && !unbounded_windows.contains(&constraint.window) => {}
                    None if policy.on_unknown == crate::policy::UnknownQuota::Admit && unbounded_windows.contains(&constraint.window) => {}
                    _ => return invalid("invalid or unapproved unknown window budget"),
                }
                if let Some(rate) = constraint.rate_per_hour {
                    if !rate.is_finite() || rate <= 0.0 || constraint.available.is_none() || constraint.next_change.is_none() {
                        return invalid("invalid per-window pacing rate");
                    }
                    period_rate = Some(period_rate.map_or(rate, |prior| prior.min(rate)));
                }
            }
            if seen.len() != p.cost_per_request.len() || unbounded_windows.len() != decision.constraints.iter().filter(|c| c.available.is_none()).count()
                || *rate_per_hour != period_rate
            {
                return invalid("decision does not bind every cost, unknown window, and pacing rate");
            }
            let mut eligible_at = None::<DateTime<Utc>>;
            for constraint in &decision.constraints {
                let Some(rate) = constraint.rate_per_hour else { continue };
                let Some(period_end) = constraint.next_change else { return invalid("period pacing is missing its boundary") };
                let Some(pace) = pacer.pacing.iter().find(|pace| pace.key.window == constraint.window
                    && pace.key.period_end == period_end) else { continue };
                let millis = (3_600_000.0 / rate).ceil();
                let next = if millis.is_finite() && millis <= i64::MAX as f64 {
                    Duration::try_milliseconds(millis as i64).and_then(|interval| pace.admitted_at.checked_add_signed(interval))
                } else { None };
                let Some(next) = next else {
                    return record_admission(pacer, record(AdmissionState::QuotaPolicyWait, Some(profile_id), None,
                        "pacing interval exceeds the representable clock range", vec![]), Vec::new());
                };
                if now < next { eligible_at = Some(eligible_at.map_or(next, |prior| prior.max(next))); }
            }
            if let Some(next) = eligible_at {
                return record_admission(pacer, record(AdmissionState::QuotaPolicyWait, Some(profile_id), Some(next),
                    "paced to each window's sustainable rate", vec![]), Vec::new());
            }
            for constraint in &decision.constraints {
                let Some(limit) = constraint.available else { continue };
                let Some(cost) = p.cost_per_request.iter().find(|cost| cost.window == constraint.window) else { return invalid("constraint is outside the profile") };
                let Some(total) = reserved_upper(pacer, constraint).and_then(Range::point)
                    .and_then(|reserved| reserved.checked_add(Range::point(cost.amount.upper)?)) else {
                    return invalid("reservation arithmetic is invalid or overflowing");
                };
                if total.upper > limit {
                    return record_admission(pacer, record(AdmissionState::QuotaPolicyWait, Some(profile_id),
                        constraint.next_change, "window budget is reserved by outstanding requests", vec![]), Vec::new());
                }
            }
            let costs = p.cost_per_request.clone();
            let mut admission = record(AdmissionState::Admitted, Some(profile_id), None, "within quota and pacing bounds", costs.clone());
            for constraint in &decision.constraints {
                if constraint.rate_per_hour.is_none() { continue; }
                let Some(period_end) = constraint.next_change else { return invalid("period pacing is missing its boundary") };
                let key = PacingKey { window: constraint.window.clone(), period_end };
                admission.paced_windows.push(key.clone());
                if let Some(pace) = pacer.pacing.iter_mut().find(|pace| pace.key == key) {
                    pace.request_id = request_id;
                    pace.admitted_at = now;
                } else {
                    pacer.pacing.push(WindowPace { key, request_id, admitted_at: now });
                }
            }
            record_admission(pacer, admission, costs)
        }
        Choice::Local { profile_id } => {
            let Some(p) = profile(policy, profile_id) else { return invalid("decided profile no longer configured") };
            if !p.local || !decision.constraints.is_empty() { return invalid("local choice has inconsistent constraints"); }
            if !decision.availability.iter().any(|item| item.profile_id == *profile_id && item.state == BackendState::Available) {
                return invalid("selected backend was not available in the decision");
            }
            if let Some(outcome) = unavailable(availability, profile_id) { return outcome; }
            let admission = record(AdmissionState::Admitted, Some(profile_id), None, "local profile draws no provider quota", vec![]);
            record_admission(pacer, admission, Vec::new())
        }
        Choice::Wait { until } if until.is_some_and(|at| at <= now) => {
            record_admission(pacer, record(AdmissionState::Eligible, None, None, "window boundary passed; recompute", vec![]), Vec::new())
        }
        Choice::Wait { until } => record_admission(pacer, record(AdmissionState::QuotaPolicyWait, None, *until, "budget returns after reset", vec![]), Vec::new()),
        Choice::ProviderExhausted { until, .. } if until.is_some_and(|at| at <= now) => {
            record_admission(pacer, record(AdmissionState::Eligible, None, None, "window boundary passed; recompute", vec![]), Vec::new())
        }
        Choice::ProviderExhausted { until, .. } => {
            record_admission(pacer, record(AdmissionState::ProviderExhausted, None, *until, "provider reports the limit reached", vec![]), Vec::new())
        }
        Choice::ObservationBlocked { .. } => {
            record_admission(pacer, record(AdmissionState::ObservationBlocked, None, None, "needed quota unobserved; policy blocks", vec![]), Vec::new())
        }
        Choice::NoBackend | Choice::CapacityUnknown { .. } => {
            let mut unknown = false;
            for profile in &policy.profiles {
                match availability.get(&profile.profile_id).copied().unwrap_or(BackendState::Unknown) {
                    BackendState::Available => return invalid("backend availability changed; recompute decision"),
                    BackendState::Unknown => unknown = true,
                    BackendState::Unavailable => {}
                }
            }
            if unknown {
                AdmissionOutcome::CapacityUnknown { reason: "backend availability is not fully observed".into() }
            } else {
                AdmissionOutcome::CapacityBlocked { reason: "no configured backend can run".into() }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchEvidence {
    NeverDispatched,
    ArmedOrUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmError {
    MissingReservation,
    IdentityMismatch,
    InvalidState,
    AttemptMismatch,
    RevisionOverflow,
}

impl std::fmt::Display for ArmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "quota arm error: {self:?}")
    }
}

impl std::error::Error for ArmError {}

/// Bind the original durable attempt for each quota window before dispatch.
/// A retry of the same binding is idempotent; a different binding is rejected.
pub fn mark_armed(pacer: &mut Pacer, admission: &QuotaAdmission, attempts: &[LocalAttempt]) -> Result<QuotaAdmission, ArmError> {
    let Some(reservation) = pacer.reservations.get_mut(&admission.request_id) else { return Err(ArmError::MissingReservation) };
    if reservation.admission.admission_id != admission.admission_id
        || reservation.admission.decision_id != admission.decision_id
    {
        return Err(ArmError::IdentityMismatch);
    }
    if reservation.admission.state != AdmissionState::Admitted { return Err(ArmError::InvalidState); }
    if attempts.len() != reservation.admission.estimated_cost.len() { return Err(ArmError::AttemptMismatch); }
    let mut bound = Vec::with_capacity(attempts.len());
    for cost in &reservation.admission.estimated_cost {
        let mut found = attempts.iter().filter(|attempt| attempt.request_id == admission.request_id
            && attempt.provider_id == cost.window.provider_id && attempt.account_scope_ref == cost.window.account_scope_ref
            && attempt.window_key == cost.window.window_key && attempt.unit == cost.unit);
        let Some(attempt) = found.next() else { return Err(ArmError::AttemptMismatch) };
        if found.next().is_some() || attempt.billing != AttemptBilling::Unknown
            || attempt.ledger_commit.ledger_id == EntityId::nil() || attempt.ledger_commit.seq.0 == 0
            || Range::new(attempt.cost.lower, attempt.cost.upper).is_none() || attempt.cost.upper > cost.amount.upper
        {
            return Err(ArmError::AttemptMismatch);
        }
        bound.push(BoundAttempt { window: cost.window.clone(), ledger_commit: attempt.ledger_commit });
    }
    if reservation.dispatch == Some(DispatchEvidence::ArmedOrUnknown) {
        return if reservation.attempts == bound { Ok(reservation.admission.clone()) } else { Err(ArmError::AttemptMismatch) };
    }
    if reservation.dispatch.is_some() || reservation.admission != *admission { return Err(ArmError::IdentityMismatch); }
    let revision = admission.record_revision.checked_add(1).ok_or(ArmError::RevisionOverflow)?;
    reservation.attempts = bound;
    reservation.dispatch = Some(DispatchEvidence::ArmedOrUnknown);
    reservation.admission.record_revision = revision;
    Ok(reservation.admission.clone())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancellationError {
    MissingReservation,
    IdentityMismatch,
    InvalidCost,
    InvalidState,
    MissingAttempt,
    RevisionOverflow,
}

impl std::fmt::Display for CancellationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "quota cancellation error: {self:?}")
    }
}

impl std::error::Error for CancellationError {}

/// A never-dispatched request releases its reservation. An armed or unknown
/// request keeps an uncertain charge until same-ledger coverage is proven.
pub fn cancel(pacer: &mut Pacer, admission: &QuotaAdmission, evidence: DispatchEvidence)
    -> Result<(QuotaAdmission, Vec<WindowCost>), CancellationError>
{
    let (cancelled, retained) = {
        let Some(reservation) = pacer.reservations.get_mut(&admission.request_id) else {
            return Err(CancellationError::MissingReservation);
        };
        if reservation.admission.admission_id != admission.admission_id
            || reservation.admission.decision_id != admission.decision_id
        {
            return Err(CancellationError::IdentityMismatch);
        }
        if reservation.admission.state == AdmissionState::Cancelled {
            return Ok((reservation.admission.clone(), reservation.outstanding.clone()));
        }
        if reservation.admission != *admission { return Err(CancellationError::IdentityMismatch); }
        if evidence == DispatchEvidence::ArmedOrUnknown && admission.state != AdmissionState::Admitted {
            return Err(CancellationError::InvalidState);
        }
        if evidence == DispatchEvidence::ArmedOrUnknown && reservation.dispatch != Some(DispatchEvidence::ArmedOrUnknown) {
            return Err(CancellationError::MissingAttempt);
        }
        let revision = admission.record_revision.checked_add(1).ok_or(CancellationError::RevisionOverflow)?;
        let retained = match evidence {
            DispatchEvidence::NeverDispatched => Vec::new(),
            DispatchEvidence::ArmedOrUnknown => reservation.outstanding.iter().map(|cost| {
                let amount = Range::new(0.0, cost.amount.upper).ok_or(CancellationError::InvalidCost)?;
                Ok(WindowCost { window: cost.window.clone(), unit: cost.unit.clone(), amount })
            }).collect::<Result<Vec<_>, CancellationError>>()?,
        };
        let mut cancelled = admission.clone();
        cancelled.state = AdmissionState::Cancelled;
        cancelled.record_revision = revision;
        reservation.admission = cancelled.clone();
        reservation.outstanding = retained.clone();
        reservation.dispatch = Some(evidence);
        if evidence == DispatchEvidence::NeverDispatched { reservation.attempts.clear(); }
        (cancelled, retained)
    };
    if evidence == DispatchEvidence::NeverDispatched {
        let mut index = 0;
        while index < pacer.pacing.len() {
            if pacer.pacing[index].request_id != admission.request_id { index += 1; continue; }
            let key = &pacer.pacing[index].key;
            let previous = pacer.reservations.values().filter(|reservation|
                reservation.dispatch != Some(DispatchEvidence::NeverDispatched)
                    && reservation.admission.paced_windows.contains(key))
                .filter_map(|reservation| reservation.admission.admitted_at.map(|at| (at, reservation.admission.request_id)))
                .max_by_key(|(at, request_id)| (*at, *request_id));
            if let Some((at, request_id)) = previous {
                pacer.pacing[index].admitted_at = at;
                pacer.pacing[index].request_id = request_id;
                index += 1;
            } else {
                pacer.pacing.remove(index);
            }
        }
    }
    Ok((cancelled, retained))
}
