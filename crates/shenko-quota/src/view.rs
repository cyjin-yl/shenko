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

use crate::model::{
    AbsentWindow, Attribution, Enforcement, Freshness, Measurement, Metric, ObservationError,
    ObservationStatus, QuotaSnapshot, Range, SourceKind, WindowKind,
};
use crate::scope::ScopeIdentity;

/// The maximum number of windows one scope view renders. A provider with
/// more windows than that keeps the rest in the durable snapshot; the
/// subject's context is bounded.
pub const MAX_VIEW_WINDOWS: usize = 8;

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
            "{}|{}|{}|{:?}|{:?}|{}|{}|{}",
            self.provider_id,
            self.account_scope_ref,
            self.scope.label(),
            self.freshness,
            self.observation_status,
            windows,
            self.selected_profile.clone().unwrap_or_default(),
            self.selected_model.clone().unwrap_or_default(),
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
                "  selected profile={profile} model={model}{}",
                match &self.selection_reason {
                    Some(reason) => format!(" reason={reason}"),
                    None => String::new(),
                }
            )),
            _ => lines.push("  no profile selected by the current policy decision".to_string()),
        }
        lines.join("\n")
    }
}
