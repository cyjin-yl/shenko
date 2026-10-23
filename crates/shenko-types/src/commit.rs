//! 提交引用与持久等级 — SPEC §41.2.1, §41.3

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::ids::{EntityId, LedgerSeq};

/// 一次提交的引用 = { ledger_id, seq }。
/// seq 只在同一 ledger_id 内严格递增，允许空洞；
/// 不同 ledger 的 seq、墙钟时间不能相互比较推断全局先后。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CommitRef {
    pub ledger_id: EntityId,
    pub seq: LedgerSeq,
}

impl CommitRef {
    pub fn new(ledger_id: EntityId, seq: LedgerSeq) -> Self {
        Self { ledger_id, seq }
    }
}

impl fmt::Display for CommitRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.ledger_id, self.seq)
    }
}

/// 持久等级。SPEC §41.2.1
/// 默认 power_loss_local：目标 OS/VFS/设备正确兑现同步语义时，
/// 确认后应经受本机进程崩溃和掉电；
/// 不包括磁盘物理损坏、失去整机、未同步远端副本或外部系统操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurabilityProfile {
    /// 进程崩溃后持久，但不保证掉电。
    ProcessCrash,
    /// 本机掉电持久（在 OS/VFS/设备同步语义成立的前提下）。
    PowerLossLocal,
}

impl Default for DurabilityProfile {
    fn default() -> Self {
        Self::PowerLossLocal
    }
}

impl fmt::Display for DurabilityProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProcessCrash => write!(f, "process_crash"),
            Self::PowerLossLocal => write!(f, "power_loss_local"),
        }
    }
}

/// 提交回执 — SPEC §41.2.1
/// 返回 commit_ref、durability_profile、store_incarnation 和本次写入实体 revision。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitReceipt {
    pub commit_ref: CommitRef,
    pub durability_profile: DurabilityProfile,
    pub store_incarnation: EntityId,
    pub updated: Vec<UpdatedEntity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatedEntity {
    pub entity_id: EntityId,
    pub record_revision: crate::ids::RecordRevision,
}
