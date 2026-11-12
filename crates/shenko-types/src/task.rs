//! 任务记录 — docs/specs/durable-state.md#spec-41-4 X02
//! TaskRecord 含完整可恢复状态。

use serde::{Deserialize, Serialize};

use crate::commit::CommitRef;
use crate::ids::{ControllerEpoch, EntityId, RecordRevision, RuntimeGeneration, TaskRevision};
use crate::object::{Digest, ObjectRef};
use crate::operation::{EffectStatus, OperationOutcome, OperationState};

/// Explicit deterministic native work requested by a delegation. Identity,
/// role and authority are deliberately absent: the Cordis registry supplies them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeTaskDefinition {
    pub kind: String,
    pub definition_version: u32,
    pub input: serde_json::Value,
}

/// Runtime assignment is separate from the immutable semantic TaskOwner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionAssignment {
    pub worker_id: EntityId,
    pub node_id: EntityId,
    pub invocation_id: EntityId,
    pub epoch: ControllerEpoch,
    pub implementation_ref: ImplementationRef,
    pub assigned_commit: CommitRef,
}

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
    Blocked {
        reason_ref: EntityId,
    },
    Parked {
        reason_ref: EntityId,
        #[serde(skip_serializing_if = "Option::is_none")]
        until_utc: Option<chrono::DateTime<chrono::Utc>>,
    },
}

/// 任务状态，docs/specs/durable-state.md#spec-41-4 X02
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

/// 完整 TaskRecord — docs/specs/durable-state.md#spec-41-4 X02
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
    /// The subject's own root task: self-owned, never dispatched by the
    /// scheduler, created atomically with subject genesis so the launch
    /// binding is part of the birth transaction (docs/specs/subject-runtime.md#spec-41-5, docs/specs/plugins.md#spec-44-6).
    pub fn subject_root(
        task_id: EntityId,
        agent_id: EntityId,
        input_ref: ObjectRef,
        created_commit: CommitRef,
    ) -> Self {
        TaskRecord {
            record_schema_version: 1,
            task_id,
            task_revision: TaskRevision(0),
            record_revision: RecordRevision(0),
            agent_id,
            conversation_id: agent_id,
            owner: TaskOwner::Task { id: task_id },
            background: false,
            kind: "subject_root".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: EntityId::nil(),
                artifact_digest: Digest::blake3(b"shenko.subject.root"),
                registry_revision: 1,
            },
            input_ref,
            state: TaskState::Pending { checkpoint_ref: None },
            abort_request: None,
            memos: serde_json::Map::new(),
            residual_case_ids: vec![],
            created_by_step_id: None,
            dispatch_eligibility: DispatchEligibility::Ready,
            no_progress_crashes: 0,
            first_run_at: None,
            terminal_at: None,
            created_commit,
            updated_commit: created_commit,
        }
    }

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

/// Durable result entry and subject-ready event. The evidence object is sealed
/// before T2; this SQL entry atomically binds it to task/result/commit identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskResult {
    pub schema_version: u32,
    pub result_id: EntityId,
    pub task_id: EntityId,
    pub task_revision: TaskRevision,
    pub task_record_revision: RecordRevision,
    pub invocation_id: EntityId,
    pub operation_id: EntityId,
    pub receipt_id: EntityId,
    pub outcome: TaskOutcome,
    /// Bounded normalized result projected into the subject event; raw evidence stays separate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    pub evidence_ref: ObjectRef,
    pub commit_ref: CommitRef,
}

/// Ordinary input reporting original E1 while a native operation remains
/// unresolved. This is not T2, does not settle the task and permits no replay.
pub const NATIVE_EVIDENCE_OBSERVATION_SOURCE: &str = "native-observation";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEvidenceObservation {
    pub schema_version: u32,
    pub task_id: EntityId,
    pub operation_id: EntityId,
    pub operation_name: String,
    pub operation_state: OperationState,
    pub outcome: Option<OperationOutcome>,
    pub effect_status: EffectStatus,
    pub attempt_id: EntityId,
    pub receipt_id: EntityId,
    pub raw_ref: ObjectRef,
    pub normalized_ref: Option<ObjectRef>,
    pub observed_at: chrono::DateTime<chrono::Utc>,
    pub received_commit: CommitRef,
}

/// Trusted executor context, not authority supplied by a model tool argument.
///
/// It is plain protocol data: the ledger issues it when a worker claims a
/// task, and a native task handler receives it with its invocation. It lives
/// here so that a handler can name it without depending on the ledger crate
/// (docs/specs/kernel.md#kernel-7).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskInvocation {
    pub task_id: EntityId,
    pub task_revision: TaskRevision,
    pub record_revision: RecordRevision,
    pub invocation_id: EntityId,
    pub epoch: ControllerEpoch,
    pub software_generation: RuntimeGeneration,
    pub worker_id: EntityId,
    pub node_id: EntityId,
}
