//! Binding-specific model usability observations. Probes are read-only
//! metadata evidence; only a real provider terminal is inference evidence.
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{BackendCapabilitySnapshot, ProviderBinding, ProviderCapabilities};
use crate::request::{InferenceResult, InferenceStatus, TerminationKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingUsability { Available, Unavailable, BindingRejected, Unknown }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindingProbe {
    pub binding: ProviderBinding,
    pub observed_at: chrono::DateTime<chrono::Utc>,
    pub usability: BindingUsability,
    pub reason: String,
    pub capabilities: Option<ProviderCapabilities>,
    /// Complete probe evidence, never a generated-model response.
    pub snapshot: Option<BackendCapabilitySnapshot>,
}

pub(crate) fn classify_probe(binding: ProviderBinding, snapshot: BackendCapabilitySnapshot) -> BindingProbe {
    let observed_at = chrono::Utc::now();
    let metadata = crate::broker::capabilities_from_snapshot(&binding, &snapshot);
    let (usability, reason, capabilities) = match metadata {
        Ok(capabilities) => (BindingUsability::Available,
            "the configured model was present in a valid read-only model metadata response; generation remains untested".into(), Some(capabilities)),
        Err(error) if error.is::<crate::InvalidModelMetadata>() =>
            (BindingUsability::Unknown, format!("invalid or conflicting model metadata: {error}"), None),
        Err(error) => {
            let explicit_refusal = snapshot.raw_evidence.iter().any(|document|
                document["probe_endpoint"].as_str() == Some("/v1/models")
                    && document["probe_http_status"].as_u64().is_some_and(|status|
                        matches!(status, 401 | 403 | 429 | 500..=599)));
            let complete_list = snapshot.raw_evidence.iter().any(|document|
                document.get("data").and_then(serde_json::Value::as_array).is_some()
                    && document.get("has_more").and_then(serde_json::Value::as_bool) != Some(true));
            let usability = if explicit_refusal || complete_list {
                BindingUsability::Unavailable
            } else { BindingUsability::Unknown };
            (usability, format!("configured model metadata not established: {error}; explicit_refusal={explicit_refusal}; complete_model_list={complete_list}"), None)
        }
    };
    BindingProbe { binding, observed_at, usability, reason, capabilities, snapshot: Some(snapshot) }
}

/// A malformed prompt, local cancellation, timeout or unknown execution is
/// not evidence that the model binding is unusable. Only attributed provider
/// responses can withdraw eligibility; the owner applies its failure window.
pub fn inference_usability(result: &InferenceResult) -> BindingUsability {
    match (&result.status, result.termination.kind) {
        (InferenceStatus::Success, TerminationKind::ProviderTerminal) => BindingUsability::Available,
        (InferenceStatus::Rejected { http_status }, TerminationKind::TransportRejected)
            if matches!(*http_status, 401 | 403 | 404) => BindingUsability::BindingRejected,
        (InferenceStatus::Rejected { http_status }, TerminationKind::TransportRejected)
            if matches!(*http_status, 429 | 500..=599) => BindingUsability::Unavailable,
        (InferenceStatus::Unavailable, TerminationKind::NeverDispatched) => BindingUsability::Unavailable,
        _ => BindingUsability::Unknown,
    }
}

/// Receives actual completed broker results after their original receipt is
/// durably recorded. No observer is called while a broker accounting lock is
/// held, and observer failure never changes the original request settlement.
pub trait InferenceHealthObserver: Send + Sync + 'static {
    fn observe(&self, binding: &ProviderBinding, result: &InferenceResult) -> anyhow::Result<()>;
}

pub(crate) type HealthObserverCell = Arc<parking_lot::RwLock<Option<Arc<dyn InferenceHealthObserver>>>>;

pub(crate) fn notify(cell: &HealthObserverCell, binding: &ProviderBinding, result: &InferenceResult) {
    let observer = cell.read().clone();
    if let Some(observer) = observer
        && let Err(error) = observer.observe(binding, result) {
        tracing::warn!(request_id=%result.request_id, %error, "model health observation was not retained; original inference settlement unchanged");
    }
}
