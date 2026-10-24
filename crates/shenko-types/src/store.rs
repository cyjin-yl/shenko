//! 存储 trait 接口 — SPEC §41.2


use serde::{Deserialize, Serialize};

use crate::commit::DurabilityProfile;
use crate::ids::{EntityId, LedgerSeq};
use crate::task::TaskRecord;
use crate::operation::{AttemptRecord, OperationRecord};
use crate::evidence::EvidenceReceipt;

/// 存储能力探测结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreCapabilities {
    pub sqlite_version: String,
    pub wal_mode: bool,
    pub fts5_available: bool,
    pub synchronous: String,
    pub journal_mode: String,
    pub incarnation: EntityId,
    pub durability_profile: DurabilityProfile,
}

/// 原始收件箱事件写入 — SPEC §12.1
#[derive(Debug)]
pub struct RawInboxEvent {
    pub event_id: EntityId,
    pub agent_id: EntityId,
    pub source: String,
    pub kind: String,
    pub payload_json: String,
    pub received_at_ms: i64,
}

/// 原始会话父行写入 — SPEC §12.1
/// 与其事件同一 WriteBatch 提交；已存在时必须一致，不改写原始事实。
#[derive(Debug)]
pub struct RawSession {
    pub session_id: EntityId,
    pub agent_id: EntityId,
    /// RFC 3339
    pub started_at: String,
    /// JSON-serialised session metadata
    pub data: String,
}

/// 原始会话事件写入 — SPEC §12.1
/// event_id 幂等；(session_id, seq) 只能属于一个 event_id。
#[derive(Debug)]
pub struct RawSessionEvent {
    pub event_id: EntityId,
    pub session_id: EntityId,
    pub seq: i64,
    pub event_type: String,
    pub role: String,
    /// JSON-serialised event payload
    pub data: String,
}

/// 写入批次
#[derive(Debug, Default)]
pub struct WriteBatch {
    pub task_upserts: Vec<TaskRecord>,
    pub operation_upserts: Vec<OperationRecord>,
    pub attempt_upserts: Vec<AttemptRecord>,
    pub evidence_upserts: Vec<EvidenceReceipt>,
    pub raw_objects: Vec<RawObject>,
    pub inbox_event_upserts: Vec<RawInboxEvent>,
    /// Written before `session_event_upserts` in the same transaction.
    pub raw_session_upserts: Vec<RawSession>,
    pub session_event_upserts: Vec<RawSessionEvent>,
}

impl WriteBatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.task_upserts.is_empty()
            && self.operation_upserts.is_empty()
            && self.attempt_upserts.is_empty()
            && self.evidence_upserts.is_empty()
            && self.raw_objects.is_empty()
            && self.inbox_event_upserts.is_empty()
            && self.raw_session_upserts.is_empty()
            && self.session_event_upserts.is_empty()
    }
}

/// 原始对象写入
#[derive(Debug)]
pub struct RawObject {
    pub data: bytes::Bytes,
    pub media_type: String,
    pub namespace: String,
    pub codec_name: String,
    pub codec_major: u32,
}

/// 存储 watch 事件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchEvent {
    pub commit_ref: crate::commit::CommitRef,
    pub updated_entity_ids: Vec<EntityId>,
}

/// 读取水位
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ReadWatermark {
    pub ledger_id: EntityId,
    pub through_seq: LedgerSeq,
}
