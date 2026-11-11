//! The durable quota events of docs/specs/quota-resources.md#spec-42-6.
//!
//! A quota change is a fact the subject can read and act on: an
//! observation arrived, a reading went stale, a forecast moved, a window
//! constraint was reached, a window reset, the policy changed or the chosen
//! model changed. Each event names the provider and account scope it
//! belongs to and carries the references it is based on — never a bare
//! number without its source.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;

/// The event kinds docs/specs/quota-resources.md#spec-42-6 names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaEventKind {
    /// A new observation was recorded for a provider/account scope.
    Observed,
    /// A reading passed its source lifetime and may no longer plan.
    ObservationStale,
    /// The planning view of a scope changed materially.
    ForecastChanged,
    /// A window constraint stopped an admission.
    ConstraintReached,
    /// A window's period was observed to have ended.
    ResetObserved,
    /// A policy revision was recorded.
    PolicyChanged,
    /// The model the policy selects for the next request changed.
    RoutingChanged,
}

impl QuotaEventKind {
    /// The stable name of this event kind.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Observed => "quota.observed",
            Self::ObservationStale => "quota.observation_stale",
            Self::ForecastChanged => "quota.forecast_changed",
            Self::ConstraintReached => "quota.constraint_reached",
            Self::ResetObserved => "quota.reset_observed",
            Self::PolicyChanged => "quota.policy_changed",
            Self::RoutingChanged => "model.routing_changed",
        }
    }
}

/// One durable quota event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaEvent {
    pub event_id: EntityId,
    pub provider_id: String,
    pub account_scope_ref: String,
    pub kind: QuotaEventKind,
    /// The references and values this event is based on: snapshot, forecast
    /// or decision ids, the reason, and the observed change.
    pub payload: serde_json::Value,
    pub observed_at: DateTime<Utc>,
}

impl QuotaEvent {
    pub fn new(
        provider_id: impl Into<String>,
        account_scope_ref: impl Into<String>,
        kind: QuotaEventKind,
        payload: serde_json::Value,
        observed_at: DateTime<Utc>,
    ) -> Self {
        Self {
            event_id: EntityId::new(),
            provider_id: provider_id.into(),
            account_scope_ref: account_scope_ref.into(),
            kind,
            payload,
            observed_at,
        }
    }
}
