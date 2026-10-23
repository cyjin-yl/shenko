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
