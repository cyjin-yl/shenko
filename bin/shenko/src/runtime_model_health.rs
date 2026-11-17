//! Actual configured-model observations shared by candidacy and inference
//! admission. Resident heartbeats never renew this proof, and read-only
//! metadata probes never masquerade as completed model generations.
use std::{sync::Arc, time::Duration};

use anyhow::{Result, ensure};
use shenko_broker::{BindingUsability, InferenceHealthObserver, inference_usability};
use shenko_broker::broker::{Broker, BrokerHandle};
use shenko_broker::request::{InferenceResult, ProviderBinding};
use shenko_node::{ModelEligibility, ModelUsabilityEvidence, ModelUsabilityOutcome, ModelUsabilitySource};
use shenko_store::Store;
use shenko_types::{ids::EntityId, object::Digest};
use tokio_util::sync::CancellationToken;

const PROBE_DEADLINE: Duration = Duration::from_secs(15);
const PROBE_INTERVAL: Duration = Duration::from_secs(30);
const EVIDENCE_LIFETIME_MINUTES: i64 = 15;

pub(crate) struct RuntimeModelHealth {
    store: Arc<Store>,
    profile_digest: Digest,
    binding: Option<ProviderBinding>,
    /// This handle owns a provider registry, but no dispatch loop is started
    /// for probing. It cannot accidentally consume an inference reservation.
    probe: Option<BrokerHandle>,
    eligibility: Arc<parking_lot::RwLock<ModelEligibility>>,
    changed: tokio::sync::Notify,
}

impl RuntimeModelHealth {
    pub(crate) fn eligibility(&self) -> Arc<parking_lot::RwLock<ModelEligibility>> {
        Arc::clone(&self.eligibility)
    }

    pub(crate) fn is_eligible(&self, runtime_id: EntityId) -> bool {
        self.eligibility.read().candidate(runtime_id, &self.profile_digest, None).is_some()
    }

    pub(crate) async fn wait_for_change(&self) {
        self.changed.notified().await;
    }

    /// Bind ordinary successful/explicitly rejected inference to the same
    /// state as the initial probe. A different sealed binding cannot inherit
    /// another endpoint/model's proof just because its display name matches.
    pub(crate) fn attach_broker(self: &Arc<Self>, handle: &BrokerHandle) -> Result<()> {
        ensure!(self.binding.as_ref() == Some(handle.binding()),
            "inference binding differs from the model-health observation; rebuild eligibility for the new binding");
        handle.set_health_observer(self.clone());
        Ok(())
    }

    fn retain<T: serde::Serialize>(&self, outcome: BindingUsability, source: ModelUsabilitySource,
        observed_at: chrono::DateTime<chrono::Utc>, observation: &T) -> Result<()> {
        let bytes = serde_json::to_vec(observation)?;
        let original = self.store.cas.seal_object(&bytes, "application/json", "model-health", "model-health-observation", 1)?.object;
        let valid_until = observed_at + chrono::Duration::minutes(EVIDENCE_LIFETIME_MINUTES);
        let evidence_bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version":1,"profile_digest":self.profile_digest,"binding":self.binding,
            "source":source,"outcome":outcome,"observed_at":observed_at,
            "valid_until":valid_until,"original_ref":original,
            "generation_success_claimed": matches!(&source, ModelUsabilitySource::Inference { .. })
                && outcome == BindingUsability::Available,
        }))?;
        let evidence = self.store.cas.seal_object(&evidence_bytes, "application/json", "model-health", "model-usability-evidence", 1)?.object;
        let outcome = match outcome {
            BindingUsability::Available => ModelUsabilityOutcome::Available,
            BindingUsability::Unavailable => ModelUsabilityOutcome::Unavailable,
            BindingUsability::Unknown => ModelUsabilityOutcome::Unknown,
        };
        self.eligibility.write().observe(self.profile_digest.clone(), outcome, ModelUsabilityEvidence {
            evidence_digest: evidence.digest, source, observed_at, valid_until,
        })?;
        self.changed.notify_waiters();
        self.changed.notify_one(); // Retain one notification across check/wait races.
        Ok(())
    }

    async fn probe_once(&self, cancel: &CancellationToken) {
        let Some(handle) = &self.probe else { return; };
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            result = tokio::time::timeout(PROBE_DEADLINE, handle.probe_binding()) => result,
        };
        let observed = match result {
            Ok(Ok(probe)) => self.retain(probe.usability, ModelUsabilitySource::CapabilityProbe,
                probe.observed_at, &probe),
            Ok(Err(error)) => self.retain(BindingUsability::Unknown, ModelUsabilitySource::CapabilityProbe,
                chrono::Utc::now(), &serde_json::json!({"probe_error":format!("{error:#}"),"generation_started":false})),
            Err(_) => self.retain(BindingUsability::Unknown, ModelUsabilitySource::CapabilityProbe,
                chrono::Utc::now(), &serde_json::json!({"probe_deadline_ms":PROBE_DEADLINE.as_millis(),
                    "outcome":"unknown","generation_started":false})),
        };
        if let Err(error) = observed {
            // No retained evidence means no renewed proof. Deterministic
            // recovery and factual ingress need not terminate with this read.
            tracing::warn!(%error, "model usability observation could not be retained");
        }
    }

    /// Caller owns and joins this finite-lifetime worker with its runtime.
    /// Unknown observations leave the old evidence's ORIGINAL expiry intact;
    /// only a new actual probe/result can refresh a usable observation.
    pub(crate) fn spawn_monitor(self: &Arc<Self>, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        let health = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + PROBE_INTERVAL, PROBE_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = interval.tick() => health.probe_once(&cancel).await,
                }
            }
        })
    }
}

impl InferenceHealthObserver for RuntimeModelHealth {
    fn observe(&self, binding: &ProviderBinding, result: &InferenceResult) -> Result<()> {
        if self.binding.as_ref() != Some(binding) { return Ok(()); }
        self.retain(inference_usability(result), ModelUsabilitySource::Inference { request_id: result.request_id },
            chrono::Utc::now(), result)
    }
}

/// Empty configuration returns immediately without provider I/O and remains
/// ineligible. Invalid setup/read failures likewise preserve recovery-only
/// operation. The configured profile and exact runtime binding are both bound
/// into retained evidence before usable eligibility is published.
pub(crate) async fn initial_probe(profile: &shenko_seed::ModelProfile, store: Arc<Store>,
    cancel: &CancellationToken) -> Result<Arc<RuntimeModelHealth>> {
    let profile_digest = Digest::blake3(&serde_json::to_vec(profile)?);
    let mut construction_error = None;
    let probe = if profile.configured {
        match Broker::new() {
            Ok((_broker, handle)) if profile.model.as_deref() == Some(handle.binding().model()) => Some(handle),
            Ok((_broker, handle)) => {
                construction_error = Some(format!("configured model {:?} differs from broker binding {}", profile.model, handle.binding().model()));
                None
            }
            Err(error) => { construction_error = Some(format!("{error:#}")); None }
        }
    } else { None };
    let health = Arc::new(RuntimeModelHealth {
        store, profile_digest, binding: probe.as_ref().map(|handle| handle.binding().clone()), probe,
        eligibility: Arc::new(parking_lot::RwLock::new(ModelEligibility::default())),
        changed: tokio::sync::Notify::new(),
    });
    if let Some(error) = construction_error {
        if let Err(retain_error) = health.retain(BindingUsability::Unknown, ModelUsabilitySource::CapabilityProbe,
            chrono::Utc::now(), &serde_json::json!({"configuration_error":error,"generation_started":false})) {
            tracing::warn!(%retain_error, "invalid model configuration remains ineligible without a retained diagnostic");
        }
    } else if profile.configured {
        health.probe_once(cancel).await;
    }
    Ok(health)
}
