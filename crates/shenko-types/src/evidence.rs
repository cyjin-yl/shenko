//! 证据收据与对账案例 — SPEC §41.4 X04

use serde::{Deserialize, Serialize};

use crate::commit::CommitRef;
use crate::ids::EntityId;
use crate::object::{ObjectRef, VersionRef};

/// 观察级别
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationLevel {
    TransportWritten,
    ProviderAccepted,
    RemoteCompleted,
    PostconditionVerified,
}

/// EvidenceReceipt — SPEC §41.4 X04
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceReceipt {
    pub record_schema_version: u32,
    pub receipt_id: EntityId,
    pub operation_id: Option<EntityId>,
    pub attempt_id: Option<EntityId>,
    pub source_id: String,
    pub source_instance_id: String,
    pub source_event_key: Option<String>,
    pub producer_epoch: Option<u64>,
    pub producer_schema_version: Option<String>,
    pub raw_ref: ObjectRef,
    pub decoder_ref: Option<VersionRef>,
    pub decode_status: DecodeStatus,
    pub observation_level: Option<ObservationLevel>,
    pub normalized_ref: Option<ObjectRef>,
    pub observed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub received_commit: CommitRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecodeStatus {
    Decoded,
    Uninterpreted,
    Invalid,
}

/// 对账案例 — SPEC §41.4 X04
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationCase {
    pub case_id: EntityId,
    pub operation_id: EntityId,
    pub supervisor_task_id: EntityId,
    pub reason: String,
    pub state: ReconciliationState,
    pub evidence_refs: Vec<EntityId>,
    pub next_action_ref: Option<ObjectRef>,
    pub decision_ref: Option<ObjectRef>,
    pub created_commit: CommitRef,
    pub updated_commit: CommitRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationState {
    Open,
    Working,
    Waiting,
    Resolved,
    AcknowledgedUnknown,
}

/// 主体对一笔“效果未知”的对账案例所作出的显式决定 — SPEC §41.4 X11。
///
/// 它是 S1 决定里的一个意图，不是调用方的自述：真正的 actor、provenance
/// 和决定原文由提交这一步的 Store 冻结在那个 sealed `StepDecision`
/// 对象里，case 的 `decision_ref` 直接指向它。它只表达“带着不确定性
/// 继续”，不把未知效果改写成成功，也不删除原始回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciliationAcknowledgement {
    /// 本轮 S0 里真实出现过的对账案例身份。
    pub case_id: EntityId,
    /// 决定事实的身份。同一个 case 的每一笔决定各有身份，后一笔不会
    /// 覆盖前一笔已经提交的原文。
    pub acknowledgement_id: EntityId,
    /// 这笔读法的理由停在哪里：该 case 已挂上的原始证据收据身份
    /// （观察/回执是合法的理由根，SPEC §51.1）。S1 会核对它确实是这个
    /// case 当时携带的收据，不会接受编造的引用。
    pub reason_refs: Vec<EntityId>,
    /// 剩余影响：为什么此刻可以带着这份未消除的不确定性继续别的
    /// 工作，以及后续还需要什么证据。
    pub remaining_impact: String,
}
