//! The Anthropic quota observer plugin — SPEC §42.2, §47.5.
//!
//! Four real observation surfaces, each with its own source contract:
//!
//! | surface | contract | what it proves |
//! | --- | --- | --- |
//! | Claude Code statusline | `anthropic:claude-code-statusline:v1` | the consumer subscription's five-hour and seven-day usage windows |
//! | API rate-limit headers | `anthropic:api-rate-limit-headers:v1` | one API key's token buckets, refilled continuously |
//! | management rate limits | `anthropic:management-rate-limits:v1` | the *configured* limits of an organization or workspace |
//! | management usage and cost | `anthropic:management-usage-cost:v1` | *reported* usage and billing of a period, with delay |
//!
//! They are deliberately not interchangeable: a configured limit is not a
//! remaining balance, a historical usage report is not a live admission
//! lock, an API token bucket is not a subscription period, and the
//! organization limit, a workspace override, the Default Workspace and one
//! API key's scope are four different scopes. Nothing here probes a
//! balance with a paid generation.
//!
//! The management documents' exact field names are version dependent. The
//! parsers therefore accept only what they can name unambiguously and leave
//! everything else unknown: an unrecognized shape yields an unavailable
//! observation, never a guessed number.

use serde_json::Value;

use crate::model::{
    AbsentReason, Enforcement, Measurement, ObservationError, ObservationStatus, QuotaSnapshot,
    QuotaWindow, SourceKind, WindowKind,
};
use crate::observe::ObservationContext;
use crate::plugin::{ObserveError, ObservationInput, ObserverDescriptor, QuotaObserver};
use crate::scope::{QuotaScope, ScopeIdentity};

pub const ANTHROPIC_PROVIDER_ID: &str = "anthropic";

/// The source contract of the management API's configured rate limits.
pub const MANAGEMENT_LIMITS_CONTRACT: &str = "anthropic:management-rate-limits:v1";

/// The source contract of the management API's usage and cost report.
pub const MANAGEMENT_USAGE_CONTRACT: &str = "anthropic:management-usage-cost:v1";

/// The Anthropic observer for one account scope and one source.
pub struct AnthropicObserver {
    descriptor: ObserverDescriptor,
}

impl AnthropicObserver {
    /// Build an observer for one account scope. The scope identity is the
    /// one this node is bound to; a reading that names a different scope is
    /// refused instead of being filed under the wrong account.
    pub fn new(
        scope: QuotaScope,
        source_kind: SourceKind,
        source_contract_ref: &str,
        source_version: Option<String>,
    ) -> Self {
        Self {
            descriptor: ObserverDescriptor {
                protocol_version: crate::plugin::QUOTA_OBSERVER_PROTOCOL_VERSION,
                provider_id: scope.provider_id.clone(),
                source_kind,
                source_contract_ref: source_contract_ref.to_string(),
                source_version,
                scope,
            },
        }
    }
}

impl QuotaObserver for AnthropicObserver {
    fn descriptor(&self) -> &ObserverDescriptor {
        &self.descriptor
    }

    fn observe(&self, input: &ObservationInput) -> Result<QuotaSnapshot, ObserveError> {
        let context = ObservationContext {
            provider_id: self.descriptor.provider_id.clone(),
            account_scope_ref: self.descriptor.scope.scope_ref.clone(),
            source_version: self.descriptor.source_version.clone(),
            attribution: crate::model::Attribution::Unknown,
            observed_at: chrono::Utc::now(),
        };
        match input {
            ObservationInput::Unavailable { error, reason } => {
                // A failed read is an unavailable observation, never a
                // balance: the missing windows keep their reason and the
                // account keeps its own scope.
                let mut snapshot = crate::observe::observation_unavailable(
                    &context,
                    self.descriptor.source_kind,
                    &self.descriptor.source_contract_ref,
                    crate::observe::expected_windows(self.descriptor.source_kind),
                    *error,
                    *reason,
                );
                snapshot.source_contract_ref = self.descriptor.source_contract_ref.clone();
                // The scope identity stays what the source named. A failed
                // read named nothing, so it does not become the bound
                // account's identity either: a missing field is not a
                // default identity (§47.5).
                Ok(snapshot)
            }
            ObservationInput::StatuslineJson { json, first_response_seen } => {
                if self.descriptor.source_kind != SourceKind::CliStatusline {
                    return Err(ObserveError::SourceMismatch);
                }
                let mut snapshot = crate::observe::from_statusline(&context, json, *first_response_seen);
                snapshot.source_contract_ref = self.descriptor.source_contract_ref.clone();
                Ok(snapshot)
            }
            ObservationInput::ApiHeaders(headers) => {
                if self.descriptor.source_kind != SourceKind::ApiHeaders {
                    return Err(ObserveError::SourceMismatch);
                }
                let mut snapshot = crate::observe::from_api_headers(
                    &context,
                    headers.iter().map(|(name, value)| (name.as_str(), value.as_str())),
                );
                snapshot.source_contract_ref = self.descriptor.source_contract_ref.clone();
                Ok(snapshot)
            }
            ObservationInput::ManagementLimits { json } => {
                if self.descriptor.source_kind != SourceKind::AdminApi
                    || self.descriptor.source_contract_ref != MANAGEMENT_LIMITS_CONTRACT
                {
                    return Err(ObserveError::SourceMismatch);
                }
                Ok(from_management_limits(&context, &self.descriptor.scope, json))
            }
            ObservationInput::ManagementUsage { json } => {
                if self.descriptor.source_kind != SourceKind::AdminApi
                    || self.descriptor.source_contract_ref != MANAGEMENT_USAGE_CONTRACT
                {
                    return Err(ObserveError::SourceMismatch);
                }
                Ok(from_management_usage(&context, &self.descriptor.scope, json))
            }
            ObservationInput::LocalAttempts { .. } => Err(ObserveError::SourceMismatch),
        }
    }
}

/// The scope a management document names, and whether it names one at all.
///
/// A missing workspace id is the Default Workspace only when the document
/// identifies the account it belongs to; a document that names no scope at
/// all keeps an unknown identity instead of silently becoming the default
/// one.
fn document_scope(document: &Value) -> ScopeIdentity {
    let text = |key: &str| -> Option<String> {
        document
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let organization = text("organization_id");
    match text("workspace_id") {
        Some(workspace_id) => ScopeIdentity::Workspace { workspace_id, organization_id: organization },
        None => match organization {
            Some(organization_id) => ScopeIdentity::Organization { organization_id },
            // No workspace id and no organization id: the document speaks
            // about the account it was requested for. Whether that is the
            // Default Workspace is decided by the binding, not guessed here.
            None => ScopeIdentity::Unknown,
        },
    }
}

/// The metric a configured limit name names. A name that says nothing keeps
/// no metric: the entry is recorded as unusable instead of being counted as
/// tokens or as money.
fn metric_for(name: &str) -> Option<crate::model::Metric> {
    let lowered = name.to_ascii_lowercase();
    if lowered.contains("input_tokens") || lowered.contains("input-tokens") {
        Some(crate::model::Metric::InputTokens)
    } else if lowered.contains("output_tokens") || lowered.contains("output-tokens") {
        Some(crate::model::Metric::OutputTokens)
    } else if lowered.contains("tokens") {
        Some(crate::model::Metric::Tokens)
    } else if lowered.contains("requests") {
        Some(crate::model::Metric::Requests)
    } else if lowered.contains("cost") || lowered.contains("usd") || lowered.contains("dollar") {
        Some(crate::model::Metric::Currency)
    } else if lowered.contains("percent") {
        Some(crate::model::Metric::UsagePercent)
    } else {
        None
    }
}

/// The unit a configured limit name or field names. Percentages and money
/// keep their own unit; a name that says nothing stays empty and the
/// window is recorded as invalid rather than counted in the wrong unit.
fn unit_for(name: &str, explicit: Option<&str>) -> Option<String> {
    if let Some(unit) = explicit.map(str::trim).filter(|unit| !unit.is_empty()) {
        return Some(unit.to_ascii_lowercase());
    }
    let lowered = name.to_ascii_lowercase();
    if lowered.contains("percent") || lowered.contains("usage_percent") {
        Some("percent".into())
    } else if lowered.contains("cost") || lowered.contains("usd") || lowered.contains("dollar") {
        Some("usd".into())
    } else if lowered.contains("tokens") {
        Some("tokens".into())
    } else if lowered.contains("requests") {
        Some("requests".into())
    } else {
        None
    }
}

/// One configured limit read from a management document.
struct ConfiguredLimit {
    name: String,
    value: f64,
    unit: Option<String>,
}

/// Read the configured limits of a management document. Both a `limits`
/// array of objects and a plain name → value map are accepted, because the
/// document's shape differs between versions; anything else is not a limit
/// list and stays unknown.
fn configured_limits(document: &Value) -> Option<Vec<ConfiguredLimit>> {
    let container = document.get("limits").unwrap_or(document);
    let mut limits = Vec::new();
    match container {
        Value::Array(items) => {
            for item in items {
                let object = item.as_object()?;
                let name = ["name", "metric", "type", "id", "key"]
                    .iter()
                    .find_map(|key| object.get(*key).and_then(Value::as_str))
                    .map(str::trim)
                    .filter(|name| !name.is_empty())?
                    .to_string();
                let value = ["value", "limit", "max", "max_value", "rate"]
                    .iter()
                    .find_map(|key| object.get(*key))
                    .and_then(Value::as_f64)?;
                let unit = ["unit", "units"].iter().find_map(|key| object.get(*key)).and_then(Value::as_str);
                limits.push(ConfiguredLimit { name, value, unit: unit.map(str::to_owned) });
            }
        }
        Value::Object(map) => {
            for (name, value) in map {
                let number = match value {
                    Value::Number(number) => number.as_f64(),
                    Value::Object(object) => ["value", "limit", "max"]
                        .iter()
                        .find_map(|key| object.get(*key))
                        .and_then(Value::as_f64),
                    _ => None,
                };
                let Some(number) = number else { continue };
                let unit = value
                    .as_object()
                    .and_then(|object| ["unit", "units"].iter().find_map(|key| object.get(*key)))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                limits.push(ConfiguredLimit { name: name.clone(), value: number, unit });
            }
        }
        _ => return None,
    }
    Some(limits)
}

/// Parse a management document describing *configured* rate limits.
///
/// A configured limit is the cap an account or workspace is subject to, not
/// a remaining balance: the window therefore carries the limit as its
/// capacity and no remaining amount at all. A workspace without its own
/// override inherits the organization limit, which is recorded as an
/// inheritance relation — never as "no limit".
pub fn from_management_limits(
    context: &ObservationContext,
    bound_scope: &QuotaScope,
    json: &str,
) -> QuotaSnapshot {
    let mut snapshot = empty_snapshot(context, SourceKind::AdminApi, MANAGEMENT_LIMITS_CONTRACT);
    let Ok(document) = serde_json::from_str::<Value>(json) else {
        return unavailable(&mut snapshot, ObservationError::InvalidData);
    };
    let identity = document_scope(&document);
    if !scope_matches_binding(&identity, bound_scope) {
        // The document names another scope than the one this node is bound
        // to. Filing it under the bound scope would attribute one account's
        // limits to another account.
        return unavailable(&mut snapshot, ObservationError::InvalidData);
    }
    snapshot.scope = identity.clone();
    let Some(limits) = configured_limits(&document) else {
        return unavailable(&mut snapshot, ObservationError::InvalidData);
    };
    // An inheritance relation is only recorded when the document states it.
    // The provider resolves the effective limit of the requested scope, so
    // a scope without an override of its own still carries the limit that
    // applies to it — never "no limit".
    let inherits_from = ["inherits_from", "inherited_from"]
        .iter()
        .find_map(|key| document.get(*key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let mut evidence = Vec::new();
    for limit in limits {
        let Some(unit) = unit_for(&limit.name, limit.unit.as_deref()) else {
            snapshot.absent_windows.push(crate::model::AbsentWindow {
                expected_window_key: limit.name.clone(),
                reason: AbsentReason::Invalid,
            });
            continue;
        };
        let Some(metric) = metric_for(&limit.name) else {
            snapshot.absent_windows.push(crate::model::AbsentWindow {
                expected_window_key: limit.name.clone(),
                reason: AbsentReason::Invalid,
            });
            continue;
        };
        if !limit.value.is_finite() || limit.value < 0.0 {
            snapshot.absent_windows.push(crate::model::AbsentWindow {
                expected_window_key: limit.name.clone(),
                reason: AbsentReason::Invalid,
            });
            continue;
        }
        let window_key = format!("configured:{}", limit.name);
        evidence.push(format!(
            "{window_key} value={} unit={unit} scope={}",
            limit.value,
            identity.label()
        ));
        snapshot.windows.push(QuotaWindow {
            window_key,
            scope_ref: context.account_scope_ref.clone(),
            scope: identity.clone(),
            inherits_from: inherits_from.clone(),
            // A configured limit has no period end and no remaining amount:
            // no period formula may be derived from it.
            kind: WindowKind::Unknown,
            metric,
            unit,
            used: None,
            remaining: None,
            capacity: Some(Measurement::reported(limit.value)),
            resets_at: None,
            replenishes_by: None,
            enforcement: Enforcement::ConfiguredCap,
        });
    }
    if snapshot.windows.is_empty() {
        return unavailable(&mut snapshot, ObservationError::InvalidData);
    }
    snapshot.raw_evidence = evidence.join("\n");
    snapshot
}

/// Parse a management document reporting *usage and cost*.
///
/// This is a historical report with a delivery delay: it states how much a
/// period consumed, not how much is left. It therefore carries used amounts
/// only, is informational, and is attributed to a shared account whose other
/// clients we cannot observe.
pub fn from_management_usage(
    context: &ObservationContext,
    bound_scope: &QuotaScope,
    json: &str,
) -> QuotaSnapshot {
    let mut snapshot = empty_snapshot(context, SourceKind::AdminApi, MANAGEMENT_USAGE_CONTRACT);
    snapshot.attribution = crate::model::Attribution::SharedUnobserved;
    let Ok(document) = serde_json::from_str::<Value>(json) else {
        return unavailable(&mut snapshot, ObservationError::InvalidData);
    };
    let identity = document_scope(&document);
    if !scope_matches_binding(&identity, bound_scope) {
        return unavailable(&mut snapshot, ObservationError::InvalidData);
    }
    snapshot.scope = identity.clone();
    let totals = usage_totals(&document);
    if totals.is_empty() {
        return unavailable(&mut snapshot, ObservationError::InvalidData);
    }
    let mut evidence = Vec::new();
    for (name, value, unit, metric) in totals {
        if !value.is_finite() || value < 0.0 {
            snapshot.absent_windows.push(crate::model::AbsentWindow {
                expected_window_key: name.clone(),
                reason: AbsentReason::Invalid,
            });
            continue;
        }
        let window_key = format!("usage:{name}");
        evidence.push(format!("{window_key} used={value} unit={unit} scope={}", identity.label()));
        snapshot.windows.push(QuotaWindow {
            window_key,
            scope_ref: context.account_scope_ref.clone(),
            scope: identity.clone(),
            inherits_from: None,
            // A reporting period is not a quota period: no reset time is
            // derived from it and no spending rate is planned from it.
            kind: WindowKind::Unknown,
            metric,
            unit,
            used: Some(Measurement::reported(value)),
            remaining: None,
            capacity: None,
            resets_at: None,
            replenishes_by: None,
            enforcement: Enforcement::Informational,
        });
    }
    if snapshot.windows.is_empty() {
        return unavailable(&mut snapshot, ObservationError::InvalidData);
    }
    snapshot.raw_evidence = evidence.join("\n");
    snapshot
}

/// The usage totals a report states, in the report's own units.
fn usage_totals(document: &Value) -> Vec<(String, f64, String, crate::model::Metric)> {
    let mut totals = Vec::new();
    let containers: Vec<&Value> = match document.get("results") {
        Some(Value::Array(items)) => items.iter().collect(),
        _ => vec![document],
    };
    let mut input_tokens = 0.0;
    let mut output_tokens = 0.0;
    let mut cost = 0.0;
    let mut seen_input = false;
    let mut seen_output = false;
    let mut seen_cost = false;
    for container in containers {
        let number = |keys: &[&str]| -> Option<f64> {
            keys.iter()
                .find_map(|key| container.get(*key))
                .and_then(Value::as_f64)
                .filter(|value| value.is_finite())
        };
        if let Some(value) = number(&["input_tokens", "inputTokens"]) {
            input_tokens += value;
            seen_input = true;
        }
        if let Some(value) = number(&["output_tokens", "outputTokens"]) {
            output_tokens += value;
            seen_output = true;
        }
        if let Some(value) = number(&["total_cost", "cost", "cost_usd", "totalCostUsd"]) {
            cost += value;
            seen_cost = true;
        }
    }
    if seen_input {
        totals.push((
            "input-tokens".into(),
            input_tokens,
            "tokens".into(),
            crate::model::Metric::InputTokens,
        ));
    }
    if seen_output {
        totals.push((
            "output-tokens".into(),
            output_tokens,
            "tokens".into(),
            crate::model::Metric::OutputTokens,
        ));
    }
    if seen_cost {
        totals.push(("cost".into(), cost, "usd".into(), crate::model::Metric::Currency));
    }
    totals
}

/// Whether a document's scope can be filed under the bound account.
///
/// A document that names no scope is accepted (the reading belongs to the
/// account it was requested for, but its scope identity stays unknown). A
/// document that names a different specific scope than the binding is
/// refused.
fn scope_matches_binding(identity: &ScopeIdentity, bound: &QuotaScope) -> bool {
    match identity {
        ScopeIdentity::Unknown => true,
        ScopeIdentity::DefaultWorkspace => match &bound.identity {
            ScopeIdentity::DefaultWorkspace | ScopeIdentity::Unknown => true,
            _ => false,
        },
        other => match &bound.identity {
            ScopeIdentity::Unknown => false,
            bound_identity => other == bound_identity,
        },
    }
}

fn empty_snapshot(
    context: &ObservationContext,
    source_kind: SourceKind,
    source_contract_ref: &str,
) -> QuotaSnapshot {
    QuotaSnapshot {
        snapshot_id: shenko_types::ids::EntityId::new(),
        provider_id: context.provider_id.clone(),
        account_scope_ref: context.account_scope_ref.clone(),
        source_kind,
        source_contract_ref: source_contract_ref.to_string(),
        scope: ScopeIdentity::Unknown,
        source_version: context.source_version.clone(),
        raw_evidence: String::new(),
        observed_at: context.observed_at,
        observation_status: ObservationStatus::Available,
        observation_error: None,
        attribution: crate::model::Attribution::Unknown,
        covered_through: None,
        windows: Vec::new(),
        absent_windows: Vec::new(),
    }
}

fn unavailable(snapshot: &mut QuotaSnapshot, error: ObservationError) -> QuotaSnapshot {
    snapshot.observation_status = ObservationStatus::Unavailable;
    snapshot.observation_error = Some(error);
    snapshot.windows.clear();
    snapshot.raw_evidence.clear();
    snapshot.clone()
}
