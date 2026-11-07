//! 自有版本化根 — SPEC §51.4、§10.1、§10.2.1
//!
//! §51.4：认知回滚不是把分支指针倒回旧提交。它在版本链**尾追加一个新节点**，
//! 其内容与某个历史节点完全相同，并按内容寻址复用已有对象；链和日志只增不减。
//!
//! 本模块定义那个「一份 manifest 引用三者」的根：一份根内容同时指名
//!
//! * `home` — 家园里不可变的文件树提交；
//! * `software` — core 与插件的软件槽（整版替换的版本对象，不在线改写）；
//! * `units` — `unit_current` 里 awareness / perception 等单元的当前版本；
//! * `runtime` — WASM 线性内存、执行栈与解释器对象图（[`WasmStateRef`]）。
//!
//! 根**不**包含也不撤销的东西：原始经历、日志、外部效应回执与债务。外部副作用
//! 只能对账并如实记为无法撤回（§10.1）；回滚后 `agent_id` 保持切换前的值。
//!
//! 顺序与当前的权威节点保存在账本里（`shenko-store` 的 `cognitive_root_nodes`），
//! 内容与态度行复用唯一的认知储存：根内容是一版内容，指向它的是一条态度。
//! 这里不另建第二份内容库、修订链或注册表。

use serde::{Deserialize, Serialize};

use crate::cognition::{AttitudeConfidence, ContentRecord, ReasonPointer};
use crate::commit::CommitRef;
use crate::error::ShenkoError;
use crate::ids::{ControllerEpoch, EntityId, RuntimeGeneration};
use crate::object::Digest;
use crate::subject::SubjectBranchHead;
use crate::wasm_state::WasmStateRef;

/// 根内容的 schema 名。major 变化才表示同一摘要的读法不兼容。
pub const COGNITIVE_ROOT_CODEC: &str = "shenko.cognitive.root";
pub const COGNITIVE_ROOT_CODEC_MAJOR: u32 = 1;
/// 根内容封存的 CAS namespace。
pub const COGNITIVE_ROOT_NAMESPACE: &str = "cognitive-root";
/// 家园文件清单与文件字节的 CAS namespace。
pub const HOME_CONTENT_NAMESPACE: &str = "home-content";
pub const HOME_CONTENT_CODEC: &str = "shenko.home.content";
pub const HOME_CONTENT_CODEC_MAJOR: u32 = 1;
/// 单元选择器快照的 CAS namespace。
pub const UNIT_SELECTOR_NAMESPACE: &str = "cognition-selector";
pub const UNIT_SELECTOR_CODEC: &str = "shenko.cognition.selector";
pub const UNIT_SELECTOR_CODEC_MAJOR: u32 = 1;
/// 软件槽清单（沿用 `shenko-update` 的 `SlotManifest`，不另造清单）的 CAS namespace。
pub const SOFTWARE_SLOT_NAMESPACE: &str = "software-slot";
pub const SOFTWARE_SLOT_CODEC: &str = "shenko.software.slot";
pub const SOFTWARE_SLOT_CODEC_MAJOR: u32 = 1;

/// 家园不可变文件树的提交。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HomeContentRef {
    /// 文件清单（`shenko.home.content/1`）的内容摘要。
    pub manifest: Digest,
}

/// 一个软件槽：core 与可执行插件的整版版本对象。
///
/// `manifest` 指向沿用至今的 `SlotManifest` 字节，`generation` 是激活选择器
/// 使用的代际名，`artifact_digest` 是该代实际可执行工件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoftwareSlotRef {
    pub manifest: Digest,
    pub generation: RuntimeGeneration,
    pub artifact_digest: Option<Digest>,
}

/// 认知单元选择器快照：`unit_current` 的 `unit_id → (revision, artifact_digest, enabled)`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitionSelectorRef {
    pub snapshot: Digest,
}

/// 运行状态的两种如实记录。
///
/// 捕获不到就写 [`RuntimeStateParticipant::NotCaptured`]：这是**公开缺口**，
/// 不是把执行状态留在根外的许可（§51.4）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeStateParticipant {
    /// 已经捕获：线性内存、执行栈与解释器对象图都在这个引用里。
    Captured(WasmStateRef),
    /// 没有捕获到。`detail` 说明是哪种机制缺口。
    NotCaptured(RuntimeStateGap),
}

impl RuntimeStateParticipant {
    pub fn captured(&self) -> Option<&WasmStateRef> {
        match self {
            Self::Captured(state) => Some(state),
            Self::NotCaptured(_) => None,
        }
    }

    pub fn gap(&self) -> Option<&RuntimeStateGap> {
        match self {
            Self::Captured(_) => None,
            Self::NotCaptured(gap) => Some(gap),
        }
    }
}

/// 一次如实记录的捕获缺口。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeStateGap {
    /// 哪一层没有捕获到（引擎/宿主/状态后端），以及为什么。
    pub detail: String,
}

impl RuntimeStateGap {
    pub fn new(detail: impl Into<String>) -> Self {
        Self { detail: detail.into() }
    }
}

/// 自有版本化根的一版内容。
///
/// 它就是那「一份 manifest」：同时引用家园文件、软件槽、单元选择器与运行状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitiveRootContent {
    pub schema_version: u32,
    pub home: HomeContentRef,
    pub software: SoftwareSlotRef,
    pub units: CognitionSelectorRef,
    pub runtime: RuntimeStateParticipant,
}

impl CognitiveRootContent {
    pub const SCHEMA_VERSION: u32 = 1;

    /// 只做形状检查，不宣称这些对象在本机存在；闭包由 writer 核对。
    pub fn validate(&self) -> Result<(), ShenkoError> {
        if self.schema_version != Self::SCHEMA_VERSION {
            return Err(ShenkoError::conflict(format!(
                "cognitive root schema version {} is not supported",
                self.schema_version
            )));
        }
        for (label, digest) in [
            ("home manifest", &self.home.manifest),
            ("software manifest", &self.software.manifest),
            ("unit selector snapshot", &self.units.snapshot),
        ] {
            digest.canonical_blake3_hex().ok_or_else(|| {
                ShenkoError::conflict(format!("{label} digest {} is not canonical blake3", digest.0))
            })?;
        }
        if let Some(artifact) = &self.software.artifact_digest {
            artifact.canonical_blake3_hex().ok_or_else(|| {
                ShenkoError::conflict(format!("software artifact digest {} is not canonical blake3", artifact.0))
            })?;
        }
        if self.software.generation.0.trim().is_empty() {
            return Err(ShenkoError::conflict("software generation is empty"));
        }
        match &self.runtime {
            RuntimeStateParticipant::Captured(state) => state
                .validate()
                .map_err(|detail| ShenkoError::conflict(format!("wasm state reference: {detail}")))?,
            RuntimeStateParticipant::NotCaptured(gap) => {
                if gap.detail.trim().is_empty() {
                    return Err(ShenkoError::conflict(
                        "an uncaptured runtime state must name the missing mechanism",
                    ));
                }
            }
        }
        Ok(())
    }

    /// 规范的字节序：结构体字段顺序固定，JSON 对象顺序因此稳定。
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ShenkoError> {
        serde_json::to_vec(self).map_err(|error| ShenkoError::storage(error.to_string()))
    }

    /// 内容身份。字节没变就是同一版，不另开一版（§51.1）。
    pub fn content_digest(&self) -> Result<Digest, ShenkoError> {
        Ok(Digest::blake3(&self.canonical_bytes()?))
    }
}

/// 写入时绑定的控制域。
///
/// 根写入被当前软件代际、控制器 epoch 与激活选择器 revision 围栏住：并发的
/// 代际切换会让写入冲突，而不是让根悄悄记在另一个控制器下。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootFence {
    pub generation: RuntimeGeneration,
    pub epoch: ControllerEpoch,
    pub selector_revision: u64,
}

impl RootFence {
    /// 这一栅栏是否正是该 head 与选择器所表示的控制器。
    pub fn covers(&self, head: &SubjectBranchHead, selector_revision: u64) -> bool {
        self.generation == head.software_generation
            && self.epoch == head.controller_epoch
            && self.selector_revision == selector_revision
    }
}

/// 指向版本链上一版根。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootRef {
    pub root_revision: u64,
    pub content: Digest,
}

impl RootRef {
    pub fn of(node: &CognitiveRootNode) -> Self {
        Self {
            root_revision: node.root_revision,
            content: node.content.digest().clone(),
        }
    }

    pub fn matches(&self, node: &CognitiveRootNode) -> bool {
        self.root_revision == node.root_revision && &self.content == node.content.digest()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootRequestKind {
    Capture,
    Reinstate,
}

impl RootRequestKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Capture => "capture",
            Self::Reinstate => "reinstate",
        }
    }
}

/// 一次根写入的稳定身份。重试同一个 `request_id` 只返回原来那条；
/// 换了内容再用同一个身份写就被拒绝（§41.6 的不确定提交读法）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitiveRootRequest {
    pub request_id: EntityId,
    pub agent_id: EntityId,
    pub kind: RootRequestKind,
    /// 这次写入产生的节点 revision。
    pub root_revision: u64,
    pub content: Digest,
    /// 回退请求指向的历史节点。
    pub target: Option<RootRef>,
    pub recorded_commit: CommitRef,
    pub recorded_at_ms: i64,
}

/// 一次捕获：把当前家园文件、软件槽、单元选择器与运行状态收进一版根。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitiveRootCapture {
    pub request_id: EntityId,
    pub fence: RootFence,
    pub content: CognitiveRootContent,
    /// 这条态度为什么这样写。空向量读作 `Unstated`，不补理由。
    pub reasons: Vec<ReasonPointer>,
    pub confidence: AttitudeConfidence,
    /// 这次捕获时对账过的外部效应；只记录，不代表撤回。
    pub effects: Vec<ExternalEffectDisposition>,
}

/// 一次回退：在链尾追加一个内容与所选历史节点相同的新节点。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitiveRootReinstatement {
    pub request_id: EntityId,
    pub fence: RootFence,
    pub target: RootRef,
    /// 回退的理由（风险与收益由主体说明）。至少要有指向当前版内容的指针，
    /// writer 会补上指向被取代版本的 `Content` 指针。
    pub reasons: Vec<ReasonPointer>,
    pub confidence: AttitudeConfidence,
    pub effects: Vec<ExternalEffectDisposition>,
}

/// 外部效应的处置。**没有**「撤回」这一可能取值：§10.1 的外部副作用只能
/// 对账，不能因为一次文件/内容回滚而消失。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectDisposition {
    /// 已经按真实回执对账过。
    Reconciled,
    /// 仍未知；保留为债务，不盲重发。
    Unknown,
}

/// 一条已被对账的外部效应。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalEffectDisposition {
    pub effect_id: EntityId,
    pub disposition: EffectDisposition,
}

/// 版本链上的一个节点。
///
/// `content` 是完整的内容登记（含 `ObjectRef` 的长度与 codec），不是裸摘要：
/// 回退要按同一份字节复用对象。`reinstated_from` 只在回退产生的新节点上出现。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitiveRootNode {
    pub schema_version: u32,
    pub agent_id: EntityId,
    /// 只增不减：第一版是 1，每追加一个节点加一。
    pub root_revision: u64,
    pub content: ContentRecord,
    /// 指向这版内容的态度行。
    pub attitude_id: EntityId,
    pub recorded_commit: CommitRef,
    pub fence: RootFence,
    pub reinstated_from: Option<RootRef>,
    pub recorded_at_ms: i64,
}

impl CognitiveRootNode {
    pub const SCHEMA_VERSION: u32 = 1;

    /// 这是否是一次回退产生的节点（内容与某个历史节点相同）。
    pub fn is_reinstatement(&self) -> bool {
        self.reinstated_from.is_some()
    }

    pub fn content_digest(&self) -> &Digest {
        self.content.digest()
    }
}

/// 当前的权威根：链尾节点，加上只增链的事实。
///
/// 这里不做「旧版本只露出数量与时间」的投影——那是已安装 `taste` 宏包的规则，
/// 内核只给出事实（§51.1、§51.3）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitiveRootHead {
    pub node: CognitiveRootNode,
    /// 这个主体已经追加过的节点数。
    pub node_count: u64,
    /// 其中内容互不相同的版本数。
    pub distinct_content_count: u64,
    /// 最近一次写入的机制时间。
    pub latest_recorded_at_ms: i64,
}

/// 链上的摘要视图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootChainEntry {
    pub root_revision: u64,
    pub content: Digest,
    pub reinstated_from: Option<RootRef>,
    pub recorded_at_ms: i64,
}

/// 根的回退要落到哪里。目前只有家园文件树；软件槽与运行状态由各自的
/// 消费者在真实边界上恢复（整版替换、状态重放），不在这里假装一起完成。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootApplicationScope {
    HomeFiles,
}

impl RootApplicationScope {
    pub fn name(self) -> &'static str {
        match self {
            Self::HomeFiles => "home_files",
        }
    }
}

/// 一次落地的实际结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootApplicationState {
    /// 已经按摘要逐个写出并核对过。
    Applied { roots: u32, files: u32 },
    /// 现在不能落地（例如旧 guest 仍拥有挂载），等真实边界。不是失败，也不是完成。
    Deferred { detail: String },
    /// 尝试过但没成功；`detail` 保留真实原因。
    Failed { detail: String },
}

/// 一条只追加的落地记录。同一个根可以对账多次。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootApplication {
    pub root_revision: u64,
    pub scope: RootApplicationScope,
    pub state: RootApplicationState,
    pub updated_commit: CommitRef,
}

impl RootApplication {
    /// 这条记录是否声明家园文件已经按这版根重写完成。
    pub fn applied(&self) -> bool {
        matches!(self.state, RootApplicationState::Applied { .. })
    }
}

/// 单元选择器快照的一行：`unit_current` 的确切当前版本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitSelectorEntry {
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: Digest,
    pub enabled: bool,
}

/// 单元选择器快照的字节形状（`shenko.cognition.selector/1`）。
///
/// 它只记录**选择器的事实**：哪个单元的哪个版本当前生效。单元的正文、依赖
/// 解析与能力声明仍由 node 的安装清单负责，这里不复制第二份。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitSelectorSnapshot {
    pub schema_version: u32,
    pub entries: Vec<UnitSelectorEntry>,
}

impl UnitSelectorSnapshot {
    pub const SCHEMA_VERSION: u32 = 1;

    /// 稳定字节序：先按单元 id 排序，再序列化。
    pub fn ok(mut entries: Vec<UnitSelectorEntry>) -> Self {
        entries.sort_by(|left, right| left.unit_id.cmp(&right.unit_id));
        Self {
            schema_version: Self::SCHEMA_VERSION,
            entries,
        }
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ShenkoError> {
        serde_json::to_vec(self).map_err(|error| ShenkoError::storage(error.to_string()))
    }

    pub fn content_digest(&self) -> Result<Digest, ShenkoError> {
        Ok(Digest::blake3(&self.canonical_bytes()?))
    }
}
