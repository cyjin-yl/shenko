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
/// grants no vLLM guarantee.
pub fn classify_operations(
    family: &BackendFamily,
    evidence: &[serde_json::Value],
    hard_retention: bool,
) -> Vec<OperationDeclaration> {
    let mut operations = Vec::new();
    for (name, namespace) in candidate_operations(family) {
        let declared = evidence
            .iter()
            .any(|document| document_names_operation(document, name));
        let mut operation = if declared {
            OperationDeclaration {
                name: name.to_string(),
                availability: OperationAvailability::Available,
                scope: OperationScope::Request,
                guarantee: Guarantee::BestEffort,
                native_namespace: (!namespace.is_empty()).then(|| namespace.to_string()),
                evidence: Some("backend named this operation in its probe response".into()),
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
        if hard_retention
            && matches!(
                operation.name.as_str(),
                NATIVE_VLLM_PREFIX_CACHING | NATIVE_STRATA_CONVERSATION_PARKING | NATIVE_NINFER_PREFIX_REUSE
            )
        {
            operation.scope = OperationScope::Engine;
            operation.guarantee = Guarantee::Reserved;
        }
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
