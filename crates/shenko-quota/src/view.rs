//! The subject-readable quota view — SPEC §42.6.
//!
//! The subject reads quota facts the same way it reads any other durable
//! fact: as bounded text that names its source. The view shows what is left
//! (as a range, never a single invented number), when the window recovers,
//! how old and how shared the reading is, and which model the current
//! policy decision selected and why.
//!
//! The view is a *projection*: it never becomes a source itself. A reading
//! that failed stays failed, an absent window keeps its reason, and a scope
//! the source never named stays unknown — the view renders what the
//! observation actually supports.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;

use crate::forecast::{Confidence, ForecastOptions, Planning, SourceValidity};
use crate::model::{
    AbsentWindow, Attribution, Enforcement, Freshness, Measurement, Metric, ObservationError,
    ObservationStatus, QuotaSnapshot, Range, SourceKind, WindowKind,
};
use crate::scope::ScopeIdentity;

/// The maximum number of windows one scope view renders. A provider with
/// more windows than that keeps the rest in the durable snapshot; the
/// subject's context is bounded.
pub const MAX_VIEW_WINDOWS: usize = 8;

/// The maximum number of planned windows one forecast view renders. The rest
/// stay in the durable forecast; the subject's context is bounded.
pub const MAX_PLAN_WINDOWS: usize = 8;

/// One window of a recorded forecast, as the subject reads the plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindowPlanView {
    pub window_key: String,
    pub unit: String,
    pub basis: Freshness,
    /// What the forecast could still plan in this window's own unit.
    pub remaining: Option<Range>,
    /// Own cost the reading does not reflect yet.
    pub unreflected_own_cost: Option<Range>,
    /// Consumption by other clients, when the policy holds an assumption.
    pub external_cost: Option<Range>,
    /// The planning arithmetic itself: a sustainable rate until a period end,
    /// a replenishing bucket, or why it is unknown.
    pub planning: Planning,
}

/// The recorded forecast one decision was planned from — SPEC §42.3
/// `cost_model_version`, `horizon`, `confidence` and `per_window`.
///
/// This is a read projection of a recorded fact. Reading it never recomputes,
/// re-routes or alters the decision: the subject reads which algorithm
/// produced the numbers, how far the plan reaches and what it planned per
/// window, and nothing here becomes a new source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaForecastView {
    pub forecast_id: EntityId,
    /// The reading this plan was computed from. When it is not the reading
    /// the view renders, the plan is about other evidence and says so.
    pub based_on_snapshot: EntityId,
    pub planning_source_is_displayed_reading: bool,
    pub computed_at: DateTime<Utc>,
    /// The forecast strategy that produced the plan, as `id@version`.
    /// Absent on a forecast recorded before the strategy seam: unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    /// How far ahead the recorded evidence reaches. Absent: unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub horizon: Option<chrono::Duration>,
    #[serde(default, skip_serializing_if = "Confidence::is_unknown")]
    pub confidence: Confidence,
    pub source_validity: SourceValidity,
    /// The lifetime and stale-use rules the forecast was computed under.
    pub options: ForecastOptions,
    pub windows: Vec<QuotaWindowPlanView>,
    pub unobserved: Vec<AbsentWindow>,
}

impl QuotaForecastView {
    /// Project one recorded forecast. `reading` is the snapshot id the view
    /// is rendering, so the projection can state whether the plan is about
    /// that same reading.
    pub fn of(forecast: &crate::forecast::QuotaForecast, reading: EntityId) -> Self {
        Self {
            forecast_id: forecast.forecast_id,
            based_on_snapshot: forecast.based_on_snapshot,
            planning_source_is_displayed_reading: forecast.based_on_snapshot == reading,
            computed_at: forecast.computed_at,
            strategy: forecast.cost_model_version.clone(),
            horizon: forecast.horizon,
            confidence: forecast.confidence,
            source_validity: forecast.source_validity,
            options: forecast.options,
            windows: forecast
                .windows
                .iter()
                .map(|window| QuotaWindowPlanView {
                    window_key: window.window_key.clone(),
                    unit: window.unit.clone(),
                    basis: window.basis,
                    remaining: window.remaining,
                    unreflected_own_cost: window.unreflected_own_cost,
                    external_cost: window.external_cost,
                    planning: window.planning.clone(),
                })
                .collect(),
            unobserved: forecast.unobserved.clone(),
        }
    }

    /// The material content of the plan: what a reader would notice.
    pub fn material(&self) -> String {
        let windows = self
            .windows
            .iter()
            .map(|window| format!("{}|{}|{:?}|{:?}", window.window_key, window.unit, window.basis, window.planning))
            .collect::<Vec<_>>()
            .join(";");
        format!(
            "{}|{}|{}|{:?}|{:?}|{:?}|{:?}|{}",
            self.forecast_id,
            self.based_on_snapshot,
            self.planning_source_is_displayed_reading,
            self.strategy,
            self.horizon,
            self.confidence,
            self.source_validity,
            windows,
        )
    }
}

/// One window as the subject reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindowView {
    pub window_key: String,
    /// The real scope this window constrains. Scopes are never merged.
    pub scope: ScopeIdentity,
    pub metric: Metric,
    pub unit: String,
    pub kind: WindowKind,
    pub enforcement: Enforcement,
    /// What is left, as an interval. Absent when the source did not state
    /// a remaining amount.
    pub remaining: Option<Range>,
    pub used: Option<Measurement>,
    pub capacity: Option<Measurement>,
    /// End of a period window. Never set for a token bucket.
    pub resets_at: Option<DateTime<Utc>>,
    /// When a token bucket is refilled. Never a period end.
    pub replenishes_by: Option<DateTime<Utc>>,
    /// The scope this window inherits its limit from, when the provider
    /// states the inheritance.
    pub inherits_from: Option<String>,
}

/// One account scope as the subject reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaScopeView {
    pub provider_id: String,
    pub account_scope_ref: String,
    /// The scope identity the source named. Unknown stays unknown.
    pub scope: ScopeIdentity,
    pub source_kind: SourceKind,
    /// The exact source contract this reading came from.
    pub source_contract_ref: String,
    pub source_version: Option<String>,
    pub observed_at: DateTime<Utc>,
    pub freshness: Freshness,
    pub observation_status: ObservationStatus,
    pub observation_error: Option<ObservationError>,
    /// Who else spends the same account.
    pub attribution: Attribution,
    pub windows: Vec<QuotaWindowView>,
    pub absent_windows: Vec<AbsentWindow>,
    /// The profile the current policy decision selected.
    pub selected_profile: Option<String>,
    /// The model that profile runs.
    pub selected_model: Option<String>,
    /// Why the decision selected it.
    pub selection_reason: Option<String>,
    /// The replaceable routing strategy that produced the current decision,
    /// as `id@version` (SPEC §42.1). Absent when no decision is recorded or
    /// the decision predates the strategy seam: unknown, never assumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_strategy: Option<String>,
    /// The effort level the current decision selected for that profile
    /// (SPEC §42.3). Absent means the implicit level of a pre-effort policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_effort: Option<String>,
    /// The recorded forecast the decision was planned from, with its
    /// strategy, horizon, confidence and per-window planning. Absent when no
    /// forecast is recorded for this scope: the plan is then unknown, not
    /// assumed to be the balance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forecast: Option<QuotaForecastView>,
}

impl QuotaScopeView {
    /// Build the view of one recorded snapshot. The freshness is computed
    /// by the caller, because only the policy knows how long a source's
    /// readings stay current.
    pub fn from_snapshot(snapshot: &QuotaSnapshot, freshness: Freshness) -> Self {
        Self {
            provider_id: snapshot.provider_id.clone(),
            account_scope_ref: snapshot.account_scope_ref.clone(),
            scope: snapshot.scope.clone(),
            source_kind: snapshot.source_kind,
            source_contract_ref: snapshot.source_contract_ref.clone(),
            source_version: snapshot.source_version.clone(),
            observed_at: snapshot.observed_at,
            freshness,
            observation_status: snapshot.observation_status,
            observation_error: snapshot.observation_error,
            attribution: snapshot.attribution,
            windows: snapshot
                .windows
                .iter()
                .map(|window| QuotaWindowView {
                    window_key: window.window_key.clone(),
                    scope: window.scope.clone(),
                    metric: window.metric,
                    unit: window.unit.clone(),
                    kind: window.kind,
                    enforcement: window.enforcement,
                    remaining: window.remaining_range(),
                    used: window.used,
                    capacity: window.capacity,
                    resets_at: window.resets_at,
                    replenishes_by: window.replenishes_by,
                    inherits_from: window.inherits_from.clone(),
                })
                .collect(),
            absent_windows: snapshot.absent_windows.clone(),
            selected_profile: None,
            selected_model: None,
            selection_reason: None,
            selected_strategy: None,
            selected_effort: None,
            forecast: None,
        }
    }

    /// Attach the model the current policy decision selected.
    pub fn with_selection(
        mut self,
        profile_id: Option<String>,
        model: Option<String>,
        reason: Option<String>,
    ) -> Self {
        self.selected_profile = profile_id;
        self.selected_model = model;
        self.selection_reason = reason;
        self
    }

    /// Attach the routing strategy that produced the current decision.
    pub fn with_strategy(mut self, strategy: Option<String>) -> Self {
        self.selected_strategy = strategy;
        self
    }

    /// Attach the effort level the current decision selected.
    pub fn with_effort(mut self, effort: Option<String>) -> Self {
        self.selected_effort = effort;
        self
    }

    /// Attach the recorded forecast the decision was planned from. The
    /// projection is read-only: it never recomputes or alters the decision.
    pub fn with_forecast(mut self, forecast: Option<QuotaForecastView>) -> Self {
        self.forecast = forecast;
        self
    }

    /// The material content of this view: what a reader would notice. It is
    /// the comparison point for merging repeated notifications — an
    /// unchanged view is not notified again.
    pub fn material(&self) -> String {
        let windows = self
            .windows
            .iter()
            .map(|window| {
                format!(
                    "{}|{}|{:?}|{}|{:?}|{:?}|{}|{}|{}",
                    window.window_key,
                    window.scope.label(),
                    window.metric,
                    window.unit,
                    window.enforcement,
                    window.remaining.map(|range| format!("{}..{}", range.lower, range.upper)),
                    window.resets_at.map(|at| at.to_rfc3339()).unwrap_or_default(),
                    window.replenishes_by.map(|at| at.to_rfc3339()).unwrap_or_default(),
                    window.inherits_from.clone().unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>()
            .join(";");
        format!(
            "{}|{}|{}|{:?}|{:?}|{}|{}|{}|{}|{}|{}",
            self.provider_id,
            self.account_scope_ref,
            self.scope.label(),
            self.freshness,
            self.observation_status,
            windows,
            self.selected_profile.clone().unwrap_or_default(),
            self.selected_model.clone().unwrap_or_default(),
            self.selected_strategy.clone().unwrap_or_default(),
            self.selected_effort.clone().unwrap_or_default(),
            // A plan that moved is a fact the subject may act on, so the
            // recorded planning view participates in the comparison.
            self.forecast.as_ref().map(QuotaForecastView::material).unwrap_or_default(),
        )
    }

    /// The bounded text the subject reads.
    pub fn render(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!(
            "quota {} scope={} contract={} source={:?} observed={} freshness={:?} attribution={:?} status={:?}{}",
            self.provider_id,
            self.scope.label(),
            self.source_contract_ref,
            self.source_kind,
            self.observed_at.to_rfc3339(),
            self.freshness,
            self.attribution,
            self.observation_status,
            match self.observation_error {
                Some(error) => format!(" error={error:?}"),
                None => String::new(),
            },
        ));
        if self.windows.is_empty() && self.absent_windows.is_empty() {
            lines.push("  no window was reported by this source".to_string());
        }
        for window in self.windows.iter().take(MAX_VIEW_WINDOWS) {
            lines.push(format!(
                "  {} scope={} metric={:?} unit={} kind={:?} enforcement={:?} remaining={} resets_at={} replenishes_by={}{}",
                window.window_key,
                window.scope.label(),
                window.metric,
                window.unit,
                window.kind,
                window.enforcement,
                match window.remaining {
                    Some(range) => format!("[{}, {}]", range.lower, range.upper),
                    None => "unknown".to_string(),
                },
                window.resets_at.map(|at| at.to_rfc3339()).unwrap_or_else(|| "none".into()),
                window.replenishes_by.map(|at| at.to_rfc3339()).unwrap_or_else(|| "none".into()),
                match &window.inherits_from {
                    Some(parent) => format!(" inherits_from={parent}"),
                    None => String::new(),
                },
            ));
        }
        if self.windows.len() > MAX_VIEW_WINDOWS {
            lines.push(format!(
                "  … {} further windows stay in the durable snapshot",
                self.windows.len() - MAX_VIEW_WINDOWS
            ));
        }
        for absent in &self.absent_windows {
            lines.push(format!(
                "  {} absent: {:?}",
                absent.expected_window_key, absent.reason
            ));
        }
        match (&self.selected_profile, &self.selected_model) {
            (Some(profile), Some(model)) => lines.push(format!(
                "  selected profile={profile} model={model}{}{}{}",
                match &self.selection_reason {
                    Some(reason) => format!(" reason={reason}"),
                    None => String::new(),
                },
                match &self.selected_strategy {
                    Some(strategy) => format!(" strategy={strategy}"),
                    None => String::new(),
                },
                match &self.selected_effort {
                    Some(effort) => format!(" effort={effort}"),
                    None => String::new(),
                },
            )),
            _ => lines.push(match &self.selected_strategy {
                Some(strategy) => format!(
                    "  no profile selected by the current policy decision strategy={strategy}"
                ),
                None => "  no profile selected by the current policy decision".to_string(),
            }),
        }
        // The plan the decision was computed from, read as a fact about the
        // numbers above. An absent forecast is stated as unknown.
        match &self.forecast {
            Some(plan) => {
                lines.push(format!(
                    "  plan forecast={} from={}{} strategy={} horizon={} confidence={:?} validity={:?}",
                    plan.forecast_id,
                    plan.based_on_snapshot,
                    if plan.planning_source_is_displayed_reading {
                        ""
                    } else {
                        " (not this reading)"
                    },
                    plan.strategy.as_deref().unwrap_or("unknown"),
                    plan.horizon
                        .map(|horizon| format!("{}s", horizon.num_seconds()))
                        .unwrap_or_else(|| "unknown".into()),
                    plan.confidence,
                    plan.source_validity,
                ));
                for window in plan.windows.iter().take(MAX_PLAN_WINDOWS) {
                    lines.push(format!(
                        "  plan {} unit={} basis={:?} remaining={} unreflected={} external={} {}",
                        window.window_key,
                        window.unit,
                        window.basis,
                        range_text(window.remaining),
                        range_text(window.unreflected_own_cost),
                        range_text(window.external_cost),
                        match &window.planning {
                            Planning::Period { available, per_hour, resets_at } => format!(
                                "period available={available} per_hour={per_hour} resets_at={}",
                                resets_at.to_rfc3339()
                            ),
                            Planning::Replenishing { remaining, refill_by } => format!(
                                "bucket remaining={} refill_by={}",
                                range_text(Some(*remaining)),
                                refill_by.map(|at| at.to_rfc3339()).unwrap_or_else(|| "unknown".into())
                            ),
                            Planning::Unknown { reason } => format!("unknown reason={reason}"),
                        },
                    ));
                }
                if plan.windows.len() > MAX_PLAN_WINDOWS {
                    lines.push(format!(
                        "  … {} further planned windows stay in the durable forecast",
                        plan.windows.len() - MAX_PLAN_WINDOWS
                    ));
                }
            }
            None => lines.push("  plan no forecast is recorded for this scope".to_string()),
        }
        lines.join("\n")
    }
}

/// A range as bounded text, or `unknown`.
fn range_text(range: Option<Range>) -> String {
    match range {
        Some(range) => format!("[{}, {}]", range.lower, range.upper),
        None => "unknown".to_string(),
    }
}
