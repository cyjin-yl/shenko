//! The local meter of Shenko's own quota consumption — SPEC §42.3/§42.4.
//!
//! The meter measures what *this* node spent, from the durable attempt rows
//! of the ledger. It is the only producer of a coverage proof: the meter's
//! own figure includes every attempt committed in this ledger up to the
//! commit it names, so those attempts are never charged a second time
//! against it. It says nothing about other clients of a shared account and
//! nothing about what a provider has already counted, so a provider
//! reading still keeps its own overlap interval.

use crate::forecast::{AttemptBilling, LocalAttempt};
use crate::model::{
    Basis, CoveredThrough, Enforcement, Measurement, ObservationError,
    ObservationStatus, QuotaSnapshot, QuotaWindow, SourceKind, WindowKind,
};
use crate::observe::ObservationContext;
use crate::plugin::{ObserveError, ObservationInput, ObserverDescriptor, QuotaObserver};
use crate::scope::{QuotaScope, ScopeIdentity};

/// The source contract of the locally measured usage meter.
pub const LOCAL_METER_CONTRACT: &str = "local:measured-usage:v1";

/// The meter of one provider/account scope.
pub struct LocalMeterObserver {
    descriptor: ObserverDescriptor,
}

impl LocalMeterObserver {
    pub fn new(scope: QuotaScope) -> Self {
        Self {
            descriptor: ObserverDescriptor {
                protocol_version: crate::plugin::QUOTA_OBSERVER_PROTOCOL_VERSION,
                provider_id: scope.provider_id.clone(),
                source_kind: SourceKind::LocalMeter,
                source_contract_ref: LOCAL_METER_CONTRACT.to_string(),
                source_version: None,
                scope,
            },
        }
    }
}

impl QuotaObserver for LocalMeterObserver {
    fn descriptor(&self) -> &ObserverDescriptor {
        &self.descriptor
    }

    fn observe(&self, input: &ObservationInput) -> Result<QuotaSnapshot, ObserveError> {
        if let ObservationInput::Unavailable { error, reason } = input {
            // The meter's own attempts are always readable; a failed read
            // of another source never becomes a local measurement.
            if self.descriptor.source_kind != SourceKind::LocalMeter {
                return Err(ObserveError::SourceMismatch);
            }
            let context = ObservationContext {
                provider_id: self.descriptor.provider_id.clone(),
                account_scope_ref: self.descriptor.scope.scope_ref.clone(),
                source_version: None,
                attribution: crate::model::Attribution::Exclusive,
                observed_at: chrono::Utc::now(),
            };
            let mut snapshot = crate::observe::observation_unavailable(
                &context,
                SourceKind::LocalMeter,
                &self.descriptor.source_contract_ref,
                &[],
                *error,
                *reason,
            );
            snapshot.source_contract_ref = self.descriptor.source_contract_ref.clone();
            return Ok(snapshot);
        }
        let ObservationInput::LocalAttempts { attempts } = input else {
            return Err(ObserveError::SourceMismatch);
        };
        if self.descriptor.source_kind != SourceKind::LocalMeter {
            return Err(ObserveError::SourceMismatch);
        }
        let context = ObservationContext {
            provider_id: self.descriptor.provider_id.clone(),
            account_scope_ref: self.descriptor.scope.scope_ref.clone(),
            source_version: None,
            attribution: crate::model::Attribution::Exclusive,
            observed_at: chrono::Utc::now(),
        };
        Ok(local_meter_snapshot(&context, attempts))
    }
}

/// Build the meter's snapshot from the durable attempts of one scope.
///
/// Attempts of another provider or scope are ignored: the meter measures
/// one account. An attempt whose billing is unknown contributes its upper
/// bound, never a settled number.
pub fn local_meter_snapshot(
    context: &ObservationContext,
    attempts: &[LocalAttempt],
) -> QuotaSnapshot {
    let mut snapshot = QuotaSnapshot {
        snapshot_id: shenko_types::ids::EntityId::new(),
        provider_id: context.provider_id.clone(),
        account_scope_ref: context.account_scope_ref.clone(),
        source_kind: SourceKind::LocalMeter,
        source_contract_ref: LOCAL_METER_CONTRACT.to_string(),
        scope: ScopeIdentity::LocalMeter,
        source_version: None,
        raw_evidence: String::new(),
        observed_at: context.observed_at,
        observation_status: ObservationStatus::Available,
        observation_error: None,
        attribution: crate::model::Attribution::Exclusive,
        covered_through: None,
        windows: Vec::new(),
        absent_windows: Vec::new(),
    };
    let mut windows: Vec<QuotaWindow> = Vec::new();
    let mut evidence = Vec::new();
    let mut covered: Option<shenko_types::commit::CommitRef> = None;
    for attempt in attempts {
        if attempt.provider_id != context.provider_id
            || attempt.account_scope_ref != context.account_scope_ref
        {
            continue;
        }
        let Some(cost) = crate::model::Range::new(attempt.cost.lower, attempt.cost.upper) else {
            continue;
        };
        let charged = match attempt.billing {
            AttemptBilling::Settled => cost,
            // An unknown charge may have cost anything up to its bound.
            AttemptBilling::Unknown => crate::model::Range::new(0.0, cost.upper).unwrap_or(cost),
        };
        let window = match windows.iter_mut().find(|window| {
            window.window_key == attempt.window_key && window.unit == attempt.unit
        }) {
            Some(window) => window,
            None => {
                windows.push(QuotaWindow {
                    window_key: attempt.window_key.clone(),
                    scope_ref: context.account_scope_ref.clone(),
                    scope: ScopeIdentity::LocalMeter,
                    inherits_from: None,
                    kind: WindowKind::Unknown,
                    metric: crate::model::Metric::Tokens,
                    unit: attempt.unit.clone(),
                    used: None,
                    remaining: None,
                    capacity: None,
                    resets_at: None,
                    replenishes_by: None,
                    enforcement: Enforcement::Informational,
                });
                windows.last_mut().expect("the window was just pushed")
            }
        };
        let used = window.used.get_or_insert(Measurement {
            value: None,
            lower: Some(0.0),
            upper: Some(0.0),
            basis: Basis::LocallyMeasured,
            precision: None,
        });
        let lower = used.lower.unwrap_or(0.0);
        let upper = used.upper.unwrap_or(0.0);
        used.value = None;
        used.lower = Some(lower + charged.lower);
        used.upper = Some(upper + charged.upper);
        if covered.is_none_or(|current| attempt.ledger_commit.seq > current.seq) {
            covered = Some(attempt.ledger_commit);
        }
    }
    if windows.is_empty() {
        snapshot.observation_status = ObservationStatus::Unavailable;
        snapshot.observation_error = Some(ObservationError::InvalidData);
        return snapshot;
    }
    for window in &windows {
        evidence.push(format!(
            "{key} used={lower}..{upper} {unit}",
            key = window.window_key,
            lower = window.used.as_ref().and_then(|used| used.lower).unwrap_or(0.0),
            upper = window.used.as_ref().and_then(|used| used.upper).unwrap_or(0.0),
            unit = window.unit,
        ));
    }
    snapshot.windows = windows;
    snapshot.raw_evidence = evidence.join("\n");
    if let Some(commit) = covered {
        snapshot.covered_through = Some(CoveredThrough { local_ledger_commit: commit });
    }
    snapshot
}
