//! One bounded, real-provider KV capability observation.
//!
//! This is the consumer recipe for the residency discovery path: it installs
//! one declared native profile, probes the binding the way any deployment
//! does, and prints exactly what the backend reported — the capability
//! snapshot, the residency controls that were actually demonstrated, the
//! discovered pool with its provenance, and the allocation the automatic
//! policy derives from it.
//!
//! It is a probe, not a generation: it reads `/v1/models`, `/metrics` and the
//! family's own diagnostics once, never retries, and never sends a prompt. A
//! backend that exposes no metrics surface leaves the pool unknown, which is a
//! valid answer and is printed as `null`.
//!
//! Usage (the model endpoint is a deployment fact, never a default here):
//!
//! ```text
//! SHENKO_KV_PROBE_URL=http://<engine-host>:<port> \
//! SHENKO_KV_PROBE_MODEL=<served-model-id> \
//! SHENKO_KV_PROBE_FAMILY=vllm \
//! cargo run -p shenko-broker --example kv_capability_probe
//! ```
//!
//! `SHENKO_KV_PROBE_FAMILY` accepts `vllm` (default), `ninfer`, `one_cat` and
//! `strata`. `SHENKO_KV_PROBE_DOMAIN` names the physical resource domain the
//! deployment binds to (defaults to `probe.<family>`), and
//! `SHENKO_KV_PROBE_API_KEY` is passed as the binding credential without ever
//! being printed. An OpenAI-compatible deployment that carries no native
//! namespace goes through `LocalModelProvider` instead; this recipe covers the
//! four native families whose probe evidence classifies native operations.

use std::sync::Arc;

use anyhow::{Context, Result};
use shenko_broker::{
    Broker, CapacityProvenance, NativeBackendProvider, ProviderProfile, ProviderRuntime,
    RESIDENCY_WIRE_OPTIONS, ResourceDomain, ResourceDomainIdentity,
};

fn profile(family: &str, url: &str, model: &str, domain: &str) -> Result<ProviderProfile> {
    use shenko_broker::profile::native_backends;
    let profile = match family {
        "vllm" => native_backends::vllm(url, model, domain),
        "ninfer" => native_backends::ninfer(url, model, domain),
        "one_cat" => native_backends::one_cat(url, model, domain),
        "strata" => native_backends::strata(url, model, domain),
        other => anyhow::bail!(
            "unknown SHENKO_KV_PROBE_FAMILY {other:?}; use vllm, ninfer, one_cat or strata"
        ),
    };
    Ok(profile)
}

#[tokio::main]
async fn main() -> Result<()> {
    let url = std::env::var("SHENKO_KV_PROBE_URL")
        .context("SHENKO_KV_PROBE_URL is required: the engine endpoint is a deployment fact")?;
    let model = std::env::var("SHENKO_KV_PROBE_MODEL")
        .context("SHENKO_KV_PROBE_MODEL is required: the served model id is a deployment fact")?;
    let family = std::env::var("SHENKO_KV_PROBE_FAMILY").unwrap_or_else(|_| "vllm".to_string());
    let domain_id =
        std::env::var("SHENKO_KV_PROBE_DOMAIN").unwrap_or_else(|_| format!("probe.{family}"));
    let api_key = std::env::var("SHENKO_KV_PROBE_API_KEY").ok();

    let profile = profile(&family, &url, &model, &domain_id)?;
    let runtime: Arc<dyn ProviderRuntime> =
        Arc::new(NativeBackendProvider::new(profile, api_key)?);
    // A probe allocates nothing, so the compute budget is nominal and the pool
    // starts unknown: whatever the backend reports is what gets adopted.
    let domain = ResourceDomain::new(
        ResourceDomainIdentity {
            domain_id: domain_id.clone(),
            engine_identity: domain_id,
            engine_incarnation: None,
            epoch: 1,
        },
        1,
        CapacityProvenance::Unknown,
        None,
        CapacityProvenance::Unknown,
    );
    let (broker, handle) = Broker::with_runtime(runtime, domain)?;

    // `probe_capabilities` both returns the snapshot and adopts it into this
    // domain's residency pool, so the printed pool is what the broker will act
    // on rather than a document nothing consumes.
    let snapshot = broker.probe_capabilities().await?;
    let plan = handle
        .rebalance_kv()
        .map_err(|error| anyhow::anyhow!("residency rebalance failed: {error}"))?;
    let controls = handle.kv_capabilities();
    let wire_options = RESIDENCY_WIRE_OPTIONS
        .iter()
        .map(|entry| {
            (
                entry.option.to_string(),
                serde_json::json!({
                    "operation": entry.operation,
                    "demonstrated": controls.demonstrates(entry.option),
                    "availability": snapshot.availability(entry.operation),
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();

    let report = serde_json::json!({
        "endpoint": url,
        "model": model,
        "family": family,
        "capability_snapshot": snapshot,
        "observed_kv_units": snapshot.observed_kv_units(),
        "residency_controls": controls,
        "residency_wire_options": wire_options,
        "residency_pool": handle.kv_snapshot(),
        "allocation_plan": {
            "subject_units": plan.subject_units,
            "children_units": plan.children_units,
            "guarantee": plan.guarantee,
            "rationale": plan.rationale,
            "candidates_evaluated": plan.candidates_evaluated,
        },
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
