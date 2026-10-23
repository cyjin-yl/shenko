//! 任务记录 — SPEC §41.4 X02
//! TaskRecord 含完整可恢复状态。

use serde::{Deserialize, Serialize};

use crate::commit::CommitRef;
use crate::ids::{
    BranchId, ControllerEpoch, EntityId, RecordRevision, RuntimeGeneration, TaskRevision,
};
use crate::object::ObjectRef;

/// 任务所有者 — conversation 或上级 task。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TaskOwner {
    Conversation { id: EntityId },
    Task { id: EntityId },
}

impl TaskOwner {
    pub fn id(&self) -> EntityId {
        match self {
            Self::Conversation { id } | Self::Task { id } => *id,
        }
    }
}

/// 任务实现引用
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImplementationRef {
    pub plugin_id: EntityId,
    pub artifact_digest: crate::object::Digest,
    pub registry_revision: u64,
}

/// 取消请求
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbortRequest {
    pub cancel_id: EntityId,
    pub decision_ref: EntityId,
    pub requested_commit: CommitRef,
    pub reason: String,
    pub scope: AbortScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbortScope {
    /// 仅当前任务
    Task,
    /// 任务及所有非 background 子树
    SubtreeOrdinary,
    /// 任务及所有子树（含 background）
    SubtreeAll,
}

/// 调度资格
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum DispatchEligibility {
    Ready,
    Blocked { reason_ref: EntityId },
    Parked {
        reason_ref: EntityId,
        #[serde(skip_serializing_if = "Option::is_none")]
        until_utc: Option<chrono::DateTime<chrono::Utc>>,
    },
}

/// 任务状态，SPEC §41.4 X02
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum TaskState {
    Pending {
        checkpoint_ref: Option<ObjectRef>,
    },
    Running {
        checkpoint_ref: Option<ObjectRef>,
        invocation_id: EntityId,
        mode: RunMode,
        epoch: ControllerEpoch,
    },
    /// 子 task 等待，仅用于有限 child。主体主循环不使用此状态。
    Waiting {
        checkpoint_ref: Option<ObjectRef>,
        on_task_ids: Vec<EntityId>,
        join: WaitJoinPolicy,
    },
    /// 子 task 定时休眠，仅用于有限 child。
    Sleeping {
        checkpoint_ref: Option<ObjectRef>,
        until_utc: chrono::DateTime<chrono::Utc>,
    },
    Completing {
        outcome_ref: ObjectRef,
    },
    Terminal {
        outcome_ref: ObjectRef,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    Run,
    Abort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitJoinPolicy {
    /// 任意子失败立即传播
    OwnedFailFast,
    /// 等待所有子完成
    AllSettled,
}

/// 完整 TaskRecord — SPEC §41.4 X02
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub record_schema_version: u32,
    pub task_id: EntityId,
    pub task_revision: TaskRevision,
    pub record_revision: RecordRevision,
    pub agent_id: EntityId,
    pub conversation_id: EntityId,
    pub owner: TaskOwner,
    /// true = background task，仅 conversation-owned。
    pub background: bool,
    pub kind: String,
    pub definition_version: u32,
    pub implementation_ref: ImplementationRef,
    pub input_ref: ObjectRef,
    pub state: TaskState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abort_request: Option<AbortRequest>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub memos: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub residual_case_ids: Vec<EntityId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_step_id: Option<EntityId>,
    pub dispatch_eligibility: DispatchEligibility,
    pub no_progress_crashes: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_run_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_commit: CommitRef,
    pub updated_commit: CommitRef,
}

impl TaskRecord {
    pub fn is_terminal(&self) -> bool {
        matches!(self.state, TaskState::Terminal { .. })
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, TaskState::Running { .. })
    }
}

/// 任务 outcome
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    Succeeded,
    Failed { reason: String },
    Cancelled,
    Interrupted,
}
