//! Operation、Attempt 记录 — SPEC §41.4 X03
//! 一次意图不等于一次网络尝试。

use serde::{Deserialize, Serialize};

use crate::commit::CommitRef;
use crate::ids::{ControllerEpoch, EntityId, RecordRevision};
use crate::object::{Digest, ObjectRef, VersionRef};

/// 资源绑定 — SPEC §41.4 X03 注
/// 至少记录实际 provider、账号/资源 identity、runtime/node、挂载 generation、
/// 相关 observation/revision 和能力等级。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceBindings {
    pub provider: String,
    pub account_identity: String,
    pub runtime_node: String,
    pub mount_generation: u64,
    pub capability_level: String,
    pub extra: serde_json::Value,
}

/// 权威引用
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorityRef {
    pub delegation_id: EntityId,
    pub delegation_revision: RecordRevision,
    pub decision_ref: EntityId,
}

/// Operation 状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Prepared,
    InFlight,
    Reconciling,
    Resolved,
}

/// Operation outcome
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationOutcome {
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}

/// 外部效果状态 — SPEC §41.4 X03
/// failed 可伴随 partial 或 unknown，cancelled 不推导 none。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectStatus {
    None,
    Partial,
    Complete,
    Unknown,
}

/// OperationRecord — SPEC §41.4 X03
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationRecord {
    pub record_schema_version: u32,
    pub operation_id: EntityId,
    pub record_revision: RecordRevision,
    pub task_id: EntityId,
    /// (task_id, logical_key) 唯一
    pub logical_key: String,
    pub operation_name: String,
    pub input_schema_version: u32,
    pub output_schema_version: u32,
    pub final_input_ref: ObjectRef,
    pub input_digest: Digest,
    pub contract_ref: OperationContractRef,
    pub resource_bindings_ref: ObjectRef,
    pub authority_ref: AuthorityRef,
    pub acceptance_predicate_ref: ObjectRef,
    pub retry_contract_ref: ObjectRef,
    pub checkpoint_scope_ref: Option<ObjectRef>,
    pub state: OperationState,
    pub outcome: Option<OperationOutcome>,
    pub effect_status: EffectStatus,
    pub retry_of: Option<EntityId>,
    pub compensates: Option<EntityId>,
    pub possible_duplicate_of: Vec<EntityId>,
    pub prepared_commit: CommitRef,
    pub evidence_through: Option<CommitRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationContractRef {
    pub plugin_id: EntityId,
    pub artifact_digest: Digest,
    pub contract_digest: Digest,
    pub receipt_reader_ref: VersionRef,
}

/// Attempt dispatch 阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptDispatchStage {
    Armed,
    Accepted,
    Running,
    Ended,
    Unknown,
}

/// Provider 幂等 key
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderIdempotencyKey {
    pub key: String,
    pub namespace: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub argument_digest: Digest,
}

/// Provider handle
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderHandle {
    pub provider_id: EntityId,
    pub backend_instance_id: EntityId,
    pub handle_type: String,
    pub value: String,
    pub version: String,
}

/// AttemptRecord — SPEC §41.4 X03
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub record_schema_version: u32,
    pub attempt_id: EntityId,
    pub operation_id: EntityId,
    pub attempt_no: u32,
    pub invocation_id: EntityId,
    pub worker_id: EntityId,
    pub node_id: EntityId,
    pub epoch: ControllerEpoch,
    pub implementation_ref: VersionRef,
    pub dispatch_stage: AttemptDispatchStage,
    pub armed_commit: CommitRef,
    pub provider_handle: Option<ProviderHandle>,
    pub provider_idempotency: Option<ProviderIdempotencyKey>,
    pub transport_ref: Option<ObjectRef>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_observed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub final_receipt_id: Option<EntityId>,
}
