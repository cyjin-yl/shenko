//! 全局唯一标识类型 — SPEC §41.3
//! ID 不可复用；uint64/bigint 在 JSON 中编码为十进制字符串。

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// 全局唯一标识，基于 UUIDv4。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(transparent)]
pub struct EntityId(pub Uuid);

impl EntityId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn nil() -> Self {
        Self(Uuid::nil())
    }

    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    pub fn to_string(&self) -> String {
        self.0.to_string()
    }
}

impl Default for EntityId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for EntityId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

/// ledger 内严格递增的序列号，允许空洞。
/// JSON 编码为十进制字符串以避免 JS 浮点截断。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord, Default,
)]
pub struct LedgerSeq(pub u64);

impl LedgerSeq {
    pub const ZERO: Self = Self(0);

    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }

    pub fn value(self) -> u64 {
        self.0
    }
}

impl fmt::Display for LedgerSeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 控制者 epoch，同分支有效控制者交接时递增。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord, Default,
)]
pub struct ControllerEpoch(pub u64);

/// 运行时 generation，激活另一软件闭包时变化。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RuntimeGeneration(pub String);

impl RuntimeGeneration {
    pub fn new(label: impl Into<String>) -> Self {
        Self(label.into())
    }
}

impl Default for RuntimeGeneration {
    fn default() -> Self {
        Self("seed".to_string())
    }
}

impl fmt::Display for RuntimeGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 任务语义版本，改目标/委派/取消/迁移时递增。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord, Default,
)]
pub struct TaskRevision(pub u64);

impl TaskRevision {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// 记录版本 (CAS)，每次持久状态更新递增。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord, Default,
)]
pub struct RecordRevision(pub u64);

impl RecordRevision {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// seat incarnation，远端程序/会话不可连续变化时递增。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord, Default,
)]
pub struct SeatIncarnation(pub u64);

/// engine incarnation，实际后端引擎重建时递增。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord, Default,
)]
pub struct EngineIncarnation(pub u64);

/// 品牌分支 ID — 可用性分支或历史分叉。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BranchId(pub Uuid);

impl BranchId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn root() -> Self {
        Self(Uuid::nil())
    }
}

impl Default for BranchId {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_seq_next() {
        assert_eq!(LedgerSeq(5).next(), LedgerSeq(6));
    }

    #[test]
    fn entity_id_unique() {
        let a = EntityId::new();
        let b = EntityId::new();
        assert_ne!(a, b);
    }
}
RUST