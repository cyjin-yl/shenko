//! Effect contract declarations -- SPEC §36
//! Five kinds of actions: reset/restore/replay/cancel/compensate.
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectActionKind {
    Reset,
    Restore,
    Replay,
    Cancel,
    Compensate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectContract {
    pub contract_id: EntityId,
    pub plugin_id: EntityId,
    pub operation_name: String,
    pub resource_scope: Vec<ResourceScope>,
    pub supported_actions: Vec<EffectActionKind>,
    pub idempotency: IdempotencyGuarantee,
    pub external_effects: Vec<ExternalEffect>,
    pub cancel_contract: Option<CancelContract>,
    pub compensation: Option<CompensationContract>,
    pub guarantee_basis: GuaranteeBasis,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceScope {
    pub kind: String,
    pub selector: String,
    pub access: ResourceAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceAccess { Read, Write, ReadWrite, Execute, Unknown }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IdempotencyGuarantee {
    Strict { key_namespace: String, max_age_secs: u64 },
    BestEffort,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalEffect {
    pub kind: String,
    pub description: String,
    pub reversible: bool,
    pub compensation_possible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelContract {
    pub can_cancel_before_start: bool,
    pub can_cancel_in_flight: bool,
    pub cancel_confirms_stopped: bool,
    pub may_have_partial_effect: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompensationContract {
    pub compensation_operation: String,
    pub preconditions: Vec<String>,
    pub may_fail: bool,
    pub leaves_traces: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GuaranteeBasis {
    ClaimOnly,
    Tested { conditions: String, evidence_ref: Option<String> },
    Unknown,
}
