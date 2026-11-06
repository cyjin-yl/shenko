//! Backend capability snapshots — SPEC §13.6, §13.7, §41.10.
//!
//! A snapshot records what one backend binding actually showed. Four facts
//! stay apart: the implementation exists, an entry point can be called, the
//! deployment enabled it, and one request actually took effect. Only the last
//! two are evidence of a native guarantee; an accepted request whose field the
//! backend ignored is not a success.
//!
//! Every operation carries the evidence it was classified from. An operation
//! that was never probed stays `Unknown` with the reason, never `Available`.

use serde::{Deserialize, Serialize};

use crate::admission::{CapacityPool, CapacityProvenance};
use crate::profile::{BackendFamily, TransportKind};

/// Whether one operation can be used on this binding and deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationAvailability {
    Available,
    RequiresConfiguration,
    Unsupported,
    Unknown,
}

/// How strong the promise behind an operation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Guarantee {
    /// A hint the backend may honour; no residency or slot guarantee.
    Advisory,
    /// The backend applies it when it can, with no proof of exclusivity.
    BestEffort,
    /// The backend acknowledged the operation.
    Acknowledged,
    /// The backend reserved the resource for this caller.
    Reserved,
}

/// The scope an operation actually acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationScope {
    Request,
    Conversation,
    Engine,
    DevicePool,
}

/// The unit a capacity is expressed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityUnit {
    Requests,
    Tokens,
    Bytes,
    Slots,
}

/// One operation of one backend binding, with the evidence behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationDeclaration {
    pub name: String,
    pub availability: OperationAvailability,
    pub scope: OperationScope,
    pub guarantee: Guarantee,
    /// The native namespace this operation belongs to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_namespace: Option<String>,
    /// The observed evidence this classification came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// Why the classification is not stronger than it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl OperationDeclaration {
    pub fn unknown(name: &str, reason: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            availability: OperationAvailability::Unknown,
            scope: OperationScope::Request,
            guarantee: Guarantee::Advisory,
            native_namespace: None,
            evidence: None,
            reason: Some(reason.into()),
        }
    }
}

/// One measured or declared capacity of a backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityEntry {
    pub pool: CapacityPool,
    pub unit: CapacityUnit,
    /// `None` means the capacity was not observed. It is never replaced by a
    /// fabricated number, and never by zero.
    #[serde(default)]
    pub value: Option<u64>,
    pub provenance: CapacityProvenance,
}

/// The capability snapshot of one bound backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendCapabilitySnapshot {
    pub capability_id: String,
    /// Digest of the provider implementation that produced this snapshot.
    pub provider_implementation_digest: String,
    pub backend_family: BackendFamily,
    #[serde(default)]
    pub backend_build: Option<String>,
    pub transport: TransportKind,
    #[serde(default)]
    pub configuration_digest: Option<String>,
    #[serde(default)]
    pub engine_incarnation: Option<String>,
    #[serde(default)]
    pub model_fingerprint: Option<String>,
    #[serde(default)]
    pub tokenizer_fingerprint: Option<String>,
    #[serde(default)]
    pub template_fingerprint: Option<String>,
    #[serde(default)]
    pub toolset_digest: Option<String>,
    pub operations: Vec<OperationDeclaration>,
    pub capacities: Vec<CapacityEntry>,
    /// Raw probe responses, kept verbatim as the evidence behind the
    /// classifications above.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub raw_evidence: Vec<serde_json::Value>,
}

impl BackendCapabilitySnapshot {
    pub fn operation(&self, name: &str) -> Option<&OperationDeclaration> {
        self.operations.iter().find(|operation| operation.name == name)
    }

    /// True only for an operation this deployment actually showed as usable.
    pub fn supports(&self, name: &str) -> bool {
        matches!(
            self.operation(name).map(|operation| operation.availability),
            Some(OperationAvailability::Available)
        )
    }

    /// The classification of one operation, or an explicit unknown.
    pub fn availability(&self, name: &str) -> OperationAvailability {
        self.operation(name)
            .map(|operation| operation.availability)
            .unwrap_or(OperationAvailability::Unknown)
    }

    pub fn capacity(&self, pool: CapacityPool) -> Option<&CapacityEntry> {
        self.capacities.iter().find(|entry| entry.pool == pool)
    }

    /// The KV capacity this backend actually reported, if any. An unobserved
    /// KV pool stays unknown: it is never promoted to a guarantee.
    pub fn observed_kv_units(&self) -> Option<u64> {
        self.capacity(CapacityPool::ContextKv)
            .and_then(|entry| entry.value)
    }

    /// Whether this backend offers real retention/quota control. Only then may
    /// a lease claim a hard reservation.
    pub fn has_hard_retention(&self) -> bool {
        self.operations.iter().any(|operation| {
            matches!(operation.availability, OperationAvailability::Available)
                && matches!(operation.guarantee, Guarantee::Reserved | Guarantee::Acknowledged)
                && matches!(operation.scope, OperationScope::Engine | OperationScope::DevicePool)
        })
    }
}

/// Candidate operations per backend family. They are candidates, not claims:
/// each one is classified from the probe evidence, and stays `Unknown` when the
/// deployment never demonstrated it.
pub fn candidate_operations(family: &BackendFamily) -> Vec<(&'static str, &'static str)> {
    let native = match family {
        BackendFamily::NInfer => vec![
            (NATIVE_NINFER_PREFIX_REUSE, NATIVE_NINFER_NAMESPACE),
            (NATIVE_NINFER_RESPONSES_CONTINUATION, NATIVE_NINFER_NAMESPACE),
            (NATIVE_NINFER_PROMPT_CACHE_KEY, NATIVE_NINFER_NAMESPACE),
            (NATIVE_NINFER_DEVICE_STATE_SLOTS, NATIVE_NINFER_NAMESPACE),
        ],
        BackendFamily::Strata => vec![
            (NATIVE_STRATA_CONVERSATION_PARKING, NATIVE_STRATA_NAMESPACE),
            (NATIVE_STRATA_PARALLEL_DECODE, NATIVE_STRATA_NAMESPACE),
            (NATIVE_STRATA_NATIVE_EVENTS, NATIVE_STRATA_NAMESPACE),
            (NATIVE_STRATA_SLOT_SAVE_RESTORE, NATIVE_STRATA_NAMESPACE),
        ],
        BackendFamily::Vllm => vec![
            (NATIVE_VLLM_PREFIX_CACHING, NATIVE_VLLM_NAMESPACE),
            (NATIVE_VLLM_CACHE_SALT, NATIVE_VLLM_NAMESPACE),
            (NATIVE_VLLM_PRIORITY, NATIVE_VLLM_NAMESPACE),
            (NATIVE_VLLM_KV_TRANSFER_PARAMS, NATIVE_VLLM_NAMESPACE),
            (NATIVE_VLLM_KV_HINTS, NATIVE_VLLM_NAMESPACE),
        ],
        BackendFamily::OneCat => vec![
            (NATIVE_ONE_CAT_SM70_DIAGNOSTICS, NATIVE_ONE_CAT_NAMESPACE),
            (NATIVE_ONE_CAT_CACHE_READ_USAGE, NATIVE_ONE_CAT_NAMESPACE),
            (NATIVE_ONE_CAT_RESET_PREFIX_CACHE, NATIVE_ONE_CAT_NAMESPACE),
        ],
        BackendFamily::Anthropic => vec![
            (OP_TOKEN_COUNT, ""),
            (OP_MESSAGE_BATCH, ""),
        ],
        BackendFamily::OpenAiCompatible | BackendFamily::Other(_) => vec![(OP_TOKEN_COUNT, "")],
    };
    native
}

pub const OP_TOKEN_COUNT: &str = "messages.count_tokens";
pub const OP_MESSAGE_BATCH: &str = "messages.batch";
pub const OP_MODEL_METADATA: &str = "model.metadata";

pub const NATIVE_NINFER_NAMESPACE: &str = "native.ninfer";
pub const NATIVE_NINFER_PREFIX_REUSE: &str = "ninfer.prefix_reuse";
pub const NATIVE_NINFER_RESPONSES_CONTINUATION: &str = "ninfer.responses_continuation";
pub const NATIVE_NINFER_PROMPT_CACHE_KEY: &str = "ninfer.prompt_cache_key";
pub const NATIVE_NINFER_DEVICE_STATE_SLOTS: &str = "ninfer.device_state_slots";

pub const NATIVE_STRATA_NAMESPACE: &str = "native.strata";
pub const NATIVE_STRATA_CONVERSATION_PARKING: &str = "strata.conversation_parking";
pub const NATIVE_STRATA_PARALLEL_DECODE: &str = "strata.parallel_decode";
pub const NATIVE_STRATA_NATIVE_EVENTS: &str = "strata.native_events";
pub const NATIVE_STRATA_SLOT_SAVE_RESTORE: &str = "strata.slot_save_restore";

pub const NATIVE_VLLM_NAMESPACE: &str = "native.vllm";
pub const NATIVE_VLLM_PREFIX_CACHING: &str = "vllm.prefix_caching";
pub const NATIVE_VLLM_CACHE_SALT: &str = "vllm.cache_salt";
pub const NATIVE_VLLM_PRIORITY: &str = "vllm.priority";
pub const NATIVE_VLLM_KV_TRANSFER_PARAMS: &str = "vllm.kv_transfer_params";
pub const NATIVE_VLLM_KV_HINTS: &str = "vllm.kv_hints";

pub const NATIVE_ONE_CAT_NAMESPACE: &str = "native.1cat";
pub const NATIVE_ONE_CAT_SM70_DIAGNOSTICS: &str = "1cat.sm70_diagnostics";
pub const NATIVE_ONE_CAT_CACHE_READ_USAGE: &str = "1cat.cache_read_usage";
pub const NATIVE_ONE_CAT_RESET_PREFIX_CACHE: &str = "1cat.reset_prefix_cache";

/// Classify the candidate operations of one binding from what the probe
/// actually returned.
///
/// An operation counts as available only when the backend itself named it in
/// the probe evidence. Naming a native namespace in a profile is not evidence:
/// an OpenAI-compatible endpoint that happens to be labelled `native.vllm`
/// grants no vLLM guarantee. No classification here can produce a reservation:
/// a probe shows that a deployment exposed an operation, never that a request
/// reserved a resource, so the guarantee stays best effort.
pub fn classify_operations(
    family: &BackendFamily,
    evidence: &[serde_json::Value],
) -> Vec<OperationDeclaration> {
    // The declared profile family is not the only source of candidates: a
    // backend that speaks an OpenAI-compatible surface can still be a vLLM
    // engine underneath, and its own metrics prove which family it is. Only
    // verbatim evidence adds candidates — the profile name never does.
    let mut families = vec![family.clone()];
    for document in evidence {
        for line in probe_body_lines(document) {
            let Some((metric, _)) = metric_line(line.trim()) else {
                continue;
            };
            let evidenced = if metric.starts_with("vllm:") {
                BackendFamily::Vllm
            } else {
                continue;
            };
            if !families.contains(&evidenced) {
                families.push(evidenced);
            }
        }
    }
    let mut operations = Vec::new();
    for (name, namespace) in families
        .iter()
        .flat_map(|family| candidate_operations(family))
    {
        // A backend may demonstrate an operation by naming it, or by a literal
        // field of the family it reported (for example the engine's cache
        // configuration saying prefix caching is enabled). Both are evidence
        // that the deployment exposed the operation; neither is evidence that a
        // particular request took effect.
        let labelled = evidence.iter().find_map(|document| {
            document_enables_operation(document, name)
        });
        let named = evidence
            .iter()
            .any(|document| document_names_operation(document, name));
        // A figure read from the engine's own configuration is an engine-level
        // fact (the deployment enabled it), not a per-request control. The
        // guarantee stays best effort either way: an enabled cache is not a
        // reservation, and no probe shows one request took effect.
        let operation = if named || labelled.is_some() {
            OperationDeclaration {
                name: name.to_string(),
                availability: OperationAvailability::Available,
                scope: if labelled.is_some() {
                    OperationScope::Engine
                } else {
                    OperationScope::Request
                },
                guarantee: Guarantee::BestEffort,
                native_namespace: (!namespace.is_empty()).then(|| namespace.to_string()),
                evidence: Some(
                    labelled.unwrap_or_else(|| {
                        "backend named this operation in its probe response".into()
                    }),
                ),
                reason: None,
            }
        } else {
            OperationDeclaration {
                name: name.to_string(),
                availability: OperationAvailability::Unknown,
                scope: OperationScope::Request,
                guarantee: Guarantee::Advisory,
                native_namespace: (!namespace.is_empty()).then(|| namespace.to_string()),
                evidence: None,
                reason: Some(
                    "this deployment did not declare the operation in its probe response; \
                     an accepted request is not evidence that it took effect"
                        .into(),
                ),
            }
        };
        operations.push(operation);
    }
    operations
}

/// Whether a probe document names an operation. The backend may declare its
/// capabilities as an object key or inside an array of names; both are
/// literal, structural evidence.
fn document_names_operation(document: &serde_json::Value, name: &str) -> bool {
    match document {
        serde_json::Value::Object(map) => {
            if map.contains_key(name) {
                return true;
            }
            map.values().any(|value| document_names_operation(value, name))
        }
        serde_json::Value::Array(items) => {
            items.iter().any(|item| match item {
                serde_json::Value::String(text) => text == name,
                other => document_names_operation(other, name),
            })
        }
        _ => false,
    }
}

/// The verbatim samples one non-JSON probe body retained. The probe keeps them
/// exactly as the endpoint sent them, because they are the evidence a derived
/// figure or classification is read from.
fn probe_body_lines(document: &serde_json::Value) -> Vec<&str> {
    document
        .get(PROBE_BODY_KEY)
        .and_then(|body| body.get("retained_lines"))
        .and_then(|lines| lines.as_array())
        .map(|lines| lines.iter().filter_map(|line| line.as_str()).collect())
        .unwrap_or_default()
}

/// Whether a probe document shows one operation enabled through a literal
/// backend field, with the exact line it was read from.
///
/// This is the deployment-enabled rung of the evidence ladder, not proof that a
/// request took effect. Only documented fields of the metric's own family are
/// read; anything else leaves the operation `Unknown`.
fn document_enables_operation(document: &serde_json::Value, name: &str) -> Option<String> {
    for line in probe_body_lines(document) {
        let line = line.trim();
        let Some((metric, labels)) = metric_line(line) else { continue };
        let label = |key: &str| {
            labels
                .iter()
                .find(|(label, _)| *label == key)
                .map(|(_, value)| *value)
        };
        let enabled = match (metric, name) {
            // The engine's effective cache configuration, not a request field.
            (KV_CACHE_CONFIG_METRIC, NATIVE_VLLM_PREFIX_CACHING) => {
                label("enable_prefix_caching").is_some_and(|value| value.eq_ignore_ascii_case("true"))
            }
            _ => false,
        };
        if enabled {
            return Some(format!("{line} => the engine reports this operation enabled"));
        }
    }
    None
}

/// A non-JSON probe body, kept with the endpoint it came from so that a figure
/// derived from it stays auditable.
pub const PROBE_BODY_KEY: &str = "probe_body";

/// The vLLM-family cache configuration metric. Its labels carry the engine's KV
/// block geometry; the sample value itself is not used.
pub const KV_CACHE_CONFIG_METRIC: &str = "vllm:cache_config_info";

/// The metrics lines one snapshot retains. A Prometheus surface is unbounded,
/// so only the samples that can establish a residency pool figure are kept —
/// verbatim, because they are the evidence the figure came from.
pub const KV_METRICS: &[&str] = &[KV_CACHE_CONFIG_METRIC];

/// One residency-affecting native option of the sealed wire, and the operation
/// the backend has to have demonstrated for that option to mean anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResidencyWireOption {
    /// The sealed `native_options` key.
    pub option: &'static str,
    /// The capability operation that backs it.
    pub operation: &'static str,
}

/// The sealed native options that ask a backend to keep, isolate or move a
/// context (SPEC §13.6, §13.7).
///
/// Naming one is a residency request. A backend that accepted the field and
/// ignored it has not demonstrated the operation, so a lease must never become
/// a residency claim just because a payload named the option.
pub const RESIDENCY_WIRE_OPTIONS: &[ResidencyWireOption] = &[
    ResidencyWireOption {
        option: "cache_salt",
        operation: NATIVE_VLLM_CACHE_SALT,
    },
    ResidencyWireOption {
        option: "prompt_cache_key",
        operation: NATIVE_NINFER_PROMPT_CACHE_KEY,
    },
    ResidencyWireOption {
        option: "kv_transfer_params",
        operation: NATIVE_VLLM_KV_TRANSFER_PARAMS,
    },
];

/// Whether one native option is a residency request at all. Other native
/// options are passed through untouched and carry no residency meaning.
pub fn is_residency_option(option: &str) -> bool {
    RESIDENCY_WIRE_OPTIONS
        .iter()
        .any(|entry| entry.option == option)
}

/// One KV pool figure a backend reported in its own probe response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedKvCapacity {
    /// Engine-declared KV pool size in tokens.
    pub tokens: u64,
    /// How the figure was obtained. An engine-declared pool configuration is
    /// `Configured`; it is never promoted to a measurement of live occupancy.
    pub provenance: CapacityProvenance,
    /// The exact document and labels the figure came from.
    pub evidence: String,
}

impl ObservedKvCapacity {
    /// The context-residency capacity entry for one capability snapshot.
    pub fn entry(&self) -> CapacityEntry {
        CapacityEntry {
            pool: CapacityPool::ContextKv,
            unit: CapacityUnit::Tokens,
            value: Some(self.tokens),
            provenance: self.provenance,
        }
    }
}

/// Discover the context-residency pool from what the backend reported itself.
///
/// Three boundaries decide what may be read (SPEC §13.3, §13.7.3):
///
/// - A model's advertised context window (`max_model_len`, `context_length`,
///   `max_input_tokens`) is the per-request ceiling. It is never used as a
///   residency pool: one sequence window cannot keep several contexts resident.
/// - Only engine pool figures count: the vLLM-family cache configuration
///   (KV blocks x block size), SGLang's `max_total_num_tokens`, or llama.cpp's
///   `total_slots` x per-slot context.
/// - A backend that reported no recognised document leaves the pool unknown.
///   Nothing is estimated, and unknown is never turned into zero.
///
/// The built-in probes feed the Prometheus surface every adapted family
/// exposes. The JSON shapes are the same reader for an adapter that reads its
/// engine's documented status endpoint: it calls this with its own verbatim
/// evidence, so a figure always carries the document it came from. This
/// function never contacts an endpoint itself.
pub fn discover_kv_capacity(
    family: &BackendFamily,
    evidence: &[serde_json::Value],
) -> Option<ObservedKvCapacity> {
    if matches!(family, BackendFamily::Anthropic) {
        // The Messages surface reports usage, not an engine KV pool. No probe
        // response can establish residency capacity for it.
        return None;
    }
    for document in evidence {
        if let Some(figure) = prometheus_kv_figure(document) {
            return Some(figure);
        }
        if let Some(figure) = json_kv_figure(document) {
            return Some(figure);
        }
    }
    None
}

/// Read one Prometheus metrics document for a KV pool figure.
fn prometheus_kv_figure(document: &serde_json::Value) -> Option<ObservedKvCapacity> {
    for line in probe_body_lines(document) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, labels)) = metric_line(line) else { continue };
        if name != KV_CACHE_CONFIG_METRIC {
            continue;
        }
        let lookup = |key: &str| {
            labels
                .iter()
                .find(|(label, _)| *label == key)
                .map(|(_, value)| *value)
        };
        let blocks = lookup("num_gpu_blocks").and_then(|value| value.parse::<u64>().ok());
        let block_size = lookup("block_size").and_then(|value| value.parse::<u64>().ok());
        let (Some(blocks), Some(block_size)) = (blocks, block_size) else { continue };
        let Some(tokens) = blocks.checked_mul(block_size) else { continue };
        if tokens == 0 {
            continue;
        }
        return Some(ObservedKvCapacity {
            tokens,
            provenance: CapacityProvenance::Configured,
            // The whole sample line is kept verbatim: the other labels matter
            // for interpretation (cache dtype, prefix caching, hybrid mamba
            // groups) and a summary would hide them. The figure is the
            // configured block geometry of the attention KV cache group, not a
            // free-headroom measurement and not a sum across cache groups.
            evidence: format!("{line} => {tokens} configured attention KV token slots"),
        });
    }
    None
}

/// Split one Prometheus sample line into its metric name and labels.
///
/// A line that does not parse is skipped: a partially understood metrics
/// document leaves the pool unknown rather than guessed.
fn metric_line(line: &str) -> Option<(&str, Vec<(&str, &str)>)> {
    let (name, rest) = match line.find('{') {
        Some(index) => (&line[..index], Some(&line[index + 1..])),
        None => {
            let end = line.find(char::is_whitespace).unwrap_or(line.len());
            (&line[..end], None)
        }
    };
    if name.is_empty() {
        return None;
    }
    let mut labels = Vec::new();
    if let Some(rest) = rest {
        let close = rest.find('}')?;
        labels = split_labels(&rest[..close])?;
    }
    Some((name, labels))
}

/// Split a Prometheus label set into its key/value pairs.
///
/// Values are quoted and may contain commas, so a naive split on `,` would
/// misread a real cache configuration line. A set that does not parse returns
/// `None`, which leaves the derived figure unknown rather than guessed.
fn split_labels(text: &str) -> Option<Vec<(&str, &str)>> {
    let mut labels = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        let (key, tail) = rest.split_once('=')?;
        let key = key.trim();
        let tail = tail.trim_start();
        let mut chars = tail.char_indices();
        if chars.next().map(|(_, character)| character) != Some('"') {
            return None;
        }
        let mut escaped = false;
        let mut close = None;
        for (index, character) in chars {
            if escaped {
                escaped = false;
                continue;
            }
            match character {
                '\\' => escaped = true,
                '"' => {
                    close = Some(index);
                    break;
                }
                _ => {}
            }
        }
        let close = close?;
        labels.push((key, &tail[1..close]));
        rest = tail[close + 1..].trim_start();
        rest = match rest.strip_prefix(',') {
            Some(rest) => rest.trim_start(),
            None => break,
        };
    }
    Some(labels)
}

/// Read one JSON probe document for an engine pool figure.
fn json_kv_figure(document: &serde_json::Value) -> Option<ObservedKvCapacity> {
    // SGLang server info: the engine's own maximum total token pool.
    if let Some(tokens) = document
        .get("max_total_num_tokens")
        .and_then(|value| value.as_u64())
        .filter(|tokens| *tokens > 0)
    {
        return Some(ObservedKvCapacity {
            tokens,
            provenance: CapacityProvenance::Configured,
            evidence: format!("max_total_num_tokens={tokens}"),
        });
    }
    // llama.cpp /props: each slot keeps its own KV cache, so the pool bound is
    // the slot count times the per-slot context.
    let slots = document
        .get("total_slots")
        .and_then(|value| value.as_u64())
        .filter(|slots| *slots > 0);
    let per_slot = document
        .get("n_ctx_per_seq")
        .and_then(|value| value.as_u64())
        .or_else(|| {
            document
                .get("default_generation_settings")
                .and_then(|settings| settings.get("n_ctx"))
                .and_then(|value| value.as_u64())
        })
        .filter(|context| *context > 0);
    if let (Some(slots), Some(per_slot)) = (slots, per_slot) {
        let tokens = slots.checked_mul(per_slot)?;
        if tokens > 0 {
            return Some(ObservedKvCapacity {
                tokens,
                provenance: CapacityProvenance::Configured,
                evidence: format!("total_slots={slots} n_ctx_per_seq={per_slot} => {tokens} KV tokens"),
            });
        }
    }
    None
}

/// Read one backend metrics surface as probe evidence.
///
/// This is an idempotent read of a diagnostic endpoint, not a generation, and
/// it is never retried: a backend without the surface (or with a failing one)
/// simply contributes an explicit `probe_error`, which leaves the pool unknown.
///
/// A Prometheus surface is unbounded, so one snapshot keeps only the samples a
/// residency figure can be derived from — verbatim, because they are the
/// evidence — together with the total line count so a reader can see that the
/// body was filtered rather than truncated.
pub(crate) async fn metrics_evidence(
    client: &reqwest::Client,
    base_url: &str,
    authorize: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
) -> serde_json::Value {
    let path = "/metrics";
    let url = format!("{}{path}", base_url.trim_end_matches('/'));
    match authorize(client.get(url)).send().await {
        Ok(response) if response.status().is_success() => {
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            match response.text().await {
                Ok(text) => {
                    let total_lines = text.lines().count() as u64;
                    let retained_lines: Vec<&str> = text
                        .lines()
                        .filter(|line| {
                            metric_line(line.trim())
                                .is_some_and(|(name, _)| KV_METRICS.contains(&name))
                        })
                        .collect();
                    serde_json::json!({
                        PROBE_BODY_KEY: {
                            "path": path,
                            "content_type": content_type,
                            "total_lines": total_lines,
                            "retained_lines": retained_lines,
                        },
                    })
                }
                Err(error) => serde_json::json!({
                    PROBE_BODY_KEY: { "path": path, "probe_error": error.to_string() },
                }),
            }
        }
        Ok(response) => serde_json::json!({
            PROBE_BODY_KEY: { "path": path, "probe_error": format!("HTTP {}", response.status()) },
        }),
        Err(error) => serde_json::json!({
            PROBE_BODY_KEY: { "path": path, "probe_error": error.to_string() },
        }),
    }
}
