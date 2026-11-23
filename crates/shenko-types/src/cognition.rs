//! 认知原子：不可变内容、账本上的态度、理由指针
//!
//! 三件事就是全部原语，不是先定下的业务类型：
//!
//! * **内容**不可变，按摘要寻址；字节没变就不另开一版。
//! * **态度**记在账本上，指向某一版内容，带理由指针与置信；supersedes 只追加，
//!   不覆盖旧行。
//! * **理由停在哪里**决定读法：停在观察、原文或回执上读起来像 fact；停在其他内容或
//!   他者视角上读起来像 conclusion。两者是同一种内容，不是两个 kind，也没有第二种
//!   储存。
//!
//! 类型的元定义本身也是内容：最新一版定义解释旧内容，旧字节不改写，旧定义仍可按
//! 版本读到。`knowledge_kind` 保留为可学习的自由标签，只作提示，不参与上面的读法。
//!
//! 契约里没有「默认置信」这种值：主体尚未判断时是显式的未知，历史行没有记录过置信
//! 就是没有记录（[`ConfidenceState::Absent`]），两者都不填数字。
//!
//! [`ContentAttitude`] 是本机账本里的一行；[`Attitude`] 是它可跨进程、跨节点封存的
//! 一等记录，带来源、适用场合与可选时效，并只准入 [`ATTITUDE_SCHEMA_VERSION`]。
//! [`Metapackage`] 是「一组被一起安装的单元」的封存声明：成员是单元 id，子宏包也是
//! 成员；安装权威仍在 Node 的 unit store，这里不另开一套解析。

use serde::{Deserialize, Serialize};

use crate::commit::CommitRef;
use crate::error::ShenkoError;
use crate::evidence::ObservationLevel;
use crate::ids::{BranchId, EntityId};
use crate::object::{Digest, ObjectRef};

/// 一版已经登记的内容。字节在 CAS 里不可变，`object.digest` 就是它的身份。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRecord {
    pub object: ObjectRef,
    /// 账本写入这版内容的机制时间。不是时间感知能力。
    pub registered_at_ms: i64,
}

impl ContentRecord {
    pub fn digest(&self) -> &Digest {
        &self.object.digest
    }

    pub fn bytes(&self) -> u64 {
        self.object.bytes
    }
}

/// 置信：主体对这条态度有多确定。
///
/// 从第一版就带置信；主体尚未判断时记为 [`AttitudeConfidence::Unknown`]，
/// 不填默认数字，也不另造量表。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AttitudeConfidence {
    /// 主体明确记录：尚未判断。这是一个判断，不是 0。
    Unknown,
    /// 主体给出的数值把握。
    Stated { value: f64 },
}

impl AttitudeConfidence {
    pub fn unknown() -> Self {
        Self::Unknown
    }

    /// 非有限的值不是置信（也无法在 JSON 里表示），返回 `None`，绝不夹取或取默认。
    ///
    /// 这里**不**内置任何区间、尺度或归一化：数值怎么读属于可修订的内容定义或
    /// 已有领域的合同，不属于内核新造的量表。
    pub fn stated(value: f64) -> Option<Self> {
        value.is_finite().then_some(Self::Stated { value })
    }

    pub fn value(self) -> Option<f64> {
        match self {
            Self::Unknown => None,
            Self::Stated { value } => Some(value),
        }
    }

    pub fn is_known(self) -> bool {
        matches!(self, Self::Stated { .. })
    }
}

/// 一条态度行读出来的置信状态。三者必须可区分，也必须是三种不同的值：
///
/// * [`ConfidenceState::Absent`] — 该行从未记录置信（历史行、外来行）。
/// * [`ConfidenceState::Unknown`] — 主体明确记录「尚未判断」。
/// * [`ConfidenceState::Stated`] — 主体给出数值。
///
/// `Absent` 不读作 0，也不读作 `Unknown`：拿历史行冒充已判断的未知同样是伪造。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "confidence", rename_all = "snake_case")]
pub enum ConfidenceState {
    Absent,
    Unknown,
    Stated { value: f64 },
}

impl ConfidenceState {
    pub fn of(recorded: Option<AttitudeConfidence>) -> Self {
        match recorded {
            None => Self::Absent,
            Some(AttitudeConfidence::Unknown) => Self::Unknown,
            Some(AttitudeConfidence::Stated { value }) => Self::Stated { value },
        }
    }
}

/// 一次观察事件。观察属于原始经历，不是 perception 单元本身。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationRef {
    pub event_id: EntityId,
    /// 把这次观察带进来的回执（如有）。
    #[serde(default)]
    pub receipt_id: Option<EntityId>,
}

/// 原文／原件：不可变的原始字节，按摘要寻址。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    pub digest: Digest,
    /// 原始定位（URL、路径、来源说明）。定位会变，摘要不变。
    #[serde(default)]
    pub locator: Option<String>,
}

/// 证据回执（X04）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptRef {
    pub receipt_id: EntityId,
    #[serde(default)]
    pub observation_level: Option<ObservationLevel>,
}

/// 对账案例。外部效应的未知状态仍是它自己的机制记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciliationRef {
    pub case_id: EntityId,
}

/// 经由他者 perception 的理解。理由停在这份理解上，不停在自己的观察上，
/// 所以它读起来是「推测对方会这样看」，不是自己的 fact。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerspectiveRef {
    /// 被模拟或借用视角的主体标签。自由字符串，不是本地身份断言。
    pub subject: String,
    /// 所用的 perception 内容摘要（如有）。
    #[serde(default)]
    pub perception: Option<Digest>,
    /// 对那个视角的理解内容摘要。理由停在这里。
    pub understanding: Digest,
}

/// 理由停在哪里。这是读法，不是内容的 kind。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonTermination {
    /// 停在一次观察上。
    Observation(ObservationRef),
    /// 停在原文／原件上。
    Source(SourceRef),
    /// 停在回执上。
    Receipt(ReceiptRef),
    /// 停在对账案例（外部效应未知的机制记录）上。
    Reconciliation(ReconciliationRef),
    /// 停在另一条内容上。
    Content(Digest),
    /// 停在他者 perception 的理解上。
    Perspective(PerspectiveRef),
}

impl ReasonTermination {
    /// 这一处终点是否属于「原始经历／原文／回执」一侧，即读作 fact 的一侧。
    pub fn is_fact_termination(&self) -> bool {
        matches!(
            self,
            Self::Observation(_) | Self::Source(_) | Self::Receipt(_) | Self::Reconciliation(_)
        )
    }
}

/// 一条理由指针。`via_perception` 记下这条理由经由谁的 perception 得到。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasonPointer {
    pub termination: ReasonTermination,
    /// 经由哪一则 perception 内容的摘要（如有）。
    #[serde(default)]
    pub via_perception: Option<Digest>,
}

impl ReasonPointer {
    pub fn observation(event_id: EntityId, receipt_id: Option<EntityId>) -> Self {
        Self::new(ReasonTermination::Observation(ObservationRef {
            event_id,
            receipt_id,
        }))
    }

    pub fn source(digest: Digest, locator: Option<String>) -> Self {
        Self::new(ReasonTermination::Source(SourceRef { digest, locator }))
    }

    pub fn receipt(receipt_id: EntityId, observation_level: Option<ObservationLevel>) -> Self {
        Self::new(ReasonTermination::Receipt(ReceiptRef {
            receipt_id,
            observation_level,
        }))
    }

    pub fn reconciliation(case_id: EntityId) -> Self {
        Self::new(ReasonTermination::Reconciliation(ReconciliationRef {
            case_id,
        }))
    }

    pub fn content(digest: Digest) -> Self {
        Self::new(ReasonTermination::Content(digest))
    }

    pub fn perspective(
        subject: impl Into<String>,
        perception: Option<Digest>,
        understanding: Digest,
    ) -> Self {
        Self::new(ReasonTermination::Perspective(PerspectiveRef {
            subject: subject.into(),
            perception,
            understanding,
        }))
    }

    pub fn new(termination: ReasonTermination) -> Self {
        Self {
            termination,
            via_perception: None,
        }
    }

    pub fn with_perception(mut self, perception: Digest) -> Self {
        self.via_perception = Some(perception);
        self
    }

    /// 这条理由是否停在给定内容上。
    pub fn stops_at(&self, digest: &Digest) -> bool {
        matches!(&self.termination, ReasonTermination::Content(other) if other == digest)
    }

    /// 一组理由停在哪里的读法。
    ///
    /// 只要有一处理由停在其他内容或他者视角上，这条态度就读作 conclusion：
    /// 主体的推导参与了这条内容，不把它升格为自己的原始观察。没有任何理由指针时
    /// 读作 `Unstated`——不记理由不等于理由成立，也不冒充 fact。
    pub fn reading(reasons: &[Self]) -> ReasonReading {
        if reasons.is_empty() {
            return ReasonReading::Unstated;
        }
        if reasons
            .iter()
            .all(|reason| reason.termination.is_fact_termination())
        {
            ReasonReading::Fact
        } else {
            ReasonReading::Conclusion
        }
    }
}

/// 理由停在哪里的读法。它是算出来的，不是存下来的 kind。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonReading {
    Fact,
    Conclusion,
    /// 一条理由指针都没有记录。
    Unstated,
}

/// 采纳目标：内容作为哪个单元的成员被接纳 。
///
/// `unit_id` 是单元／宏包自己的 id（例如 `forgetting`）。能力合同用另一套
/// `cap.` 前缀的标识，两者不得混用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdoptionTarget {
    pub unit_id: String,
    #[serde(default)]
    pub unit_version: Option<String>,
    /// 单元里的哪个成员槽位（如有）。
    #[serde(default)]
    pub member: Option<String>,
}

impl AdoptionTarget {
    pub fn unit(unit_id: impl Into<String>) -> Self {
        Self {
            unit_id: unit_id.into(),
            unit_version: None,
            member: None,
        }
    }

    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.unit_version = Some(version.into());
        self
    }

    pub fn with_member(mut self, member: impl Into<String>) -> Self {
        self.member = Some(member.into());
        self
    }
}

/// 遗忘／投影的六个阶段。状态不改字节，只改默认露出。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionStage {
    Degraded,
    Fuzzy,
    NearForgotten,
    Archived,
    LongTerm,
    Forgotten,
}

/// 态度在账本上承担的机制角色。这不是内容的语义 kind：
/// 内容读作 fact 还是 conclusion 由 [`ReasonPointer::reading`] 决定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttitudeRole {
    /// 主体对内容的断言／判断。
    Assertion,
    /// 采纳：内容成为某个单元／宏包的成员。生成或修订本身都不是采纳。
    Adoption(AdoptionTarget),
    /// 修订：这一版取代同一条内容线的旧版本。
    /// 旧版本仍在，字节不改写；理由里必须有指向旧 digest 的 `Content` 指针。
    Revision { supersedes: Digest },
    /// 元定义：把定义槽指向这一版内容。
    /// 最新一版定义解释旧内容；旧定义按版本仍可读到。
    Definition(String),
    /// 投影状态（遗忘六阶段）。
    Projection(ProjectionStage),
    /// 尚未完成（原 `PendingObligation` 的读法）。
    Unfinished,
    /// 外部效应未知已被记账且不自动重放。
    EffectUnknown,
}

impl AttitudeRole {
    /// 账本里的角色判别列。它是机制角色名，不是内容种类。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Assertion => "assertion",
            Self::Adoption(_) => "adoption",
            Self::Revision { .. } => "revision",
            Self::Definition(_) => "definition",
            Self::Projection(_) => "projection",
            Self::Unfinished => "unfinished",
            Self::EffectUnknown => "effect_unknown",
        }
    }

    pub fn adoption_target(&self) -> Option<&AdoptionTarget> {
        match self {
            Self::Adoption(target) => Some(target),
            _ => None,
        }
    }

    pub fn definition_slot(&self) -> Option<&str> {
        match self {
            Self::Definition(slot) => Some(slot.as_str()),
            _ => None,
        }
    }

    pub fn revision_parent(&self) -> Option<&Digest> {
        match self {
            Self::Revision { supersedes } => Some(supersedes),
            _ => None,
        }
    }

    pub fn projection_stage(&self) -> Option<ProjectionStage> {
        match self {
            Self::Projection(stage) => Some(*stage),
            _ => None,
        }
    }
}

/// 账本上的一条态度。supersedes 只追加，不覆盖旧行 。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentAttitude {
    pub attitude_id: EntityId,
    /// 写下这条态度的主体。
    pub agent_id: EntityId,
    /// 指向哪一版内容。
    pub digest: Digest,
    pub role: AttitudeRole,
    /// 理由指针。空表示没有记录理由，读作 `Unstated`。
    pub reasons: Vec<ReasonPointer>,
    /// `None` = 该行没有记录置信（历史行）；`Some(Unknown)` = 主体明确尚未判断。
    pub confidence: Option<AttitudeConfidence>,
    /// 可学习的自由标签，只作提示，不是种类边界。
    pub knowledge_kind_hint: Option<String>,
    /// 这条态度取代的上一态度（不是内容）。
    pub supersedes: Option<EntityId>,
    pub created_commit: CommitRef,
    pub created_at_ms: i64,
}

impl ContentAttitude {
    pub fn reading(&self) -> ReasonReading {
        ReasonPointer::reading(&self.reasons)
    }

    pub fn confidence_state(&self) -> ConfidenceState {
        ConfidenceState::of(self.confidence)
    }

    /// 这版内容是否由这条修订态度取代的旧版本。
    pub fn revises(&self) -> Option<&Digest> {
        self.role.revision_parent()
    }
}

// ------------------------------------------- sealed first-class records ---

/// 一条封存态度的格式版本。只准入这一版；没有未版本化的别名。
pub const ATTITUDE_SCHEMA_VERSION: u32 = 1;

/// 一份封存宏包声明的格式版本。只准入这一版；没有未版本化的别名。
pub const METAPACKAGE_SCHEMA_VERSION: u32 = 1;

/// 一条记录来自哪里：哪个节点的哪次提交，或者它从哪一版字节读回。
///
/// 这是来源，不是权限。来源远在他处的记录仍是主体自己的判断，只是本机账本里没有
/// 那次提交；缺席的字段如实缺席，不拿本机提交冒充本地写入。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitionProvenance {
    /// 写下这一版的节点标签。空串不是合法来源。
    pub origin_node: String,
    /// 写节点的提交。跨节点复制时本机没有这次提交，来源仍可按它核对。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_commit: Option<CommitRef>,
    /// 这一版是从哪些字节读回的（导入／导出时的原件摘要）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_digest: Option<Digest>,
}

impl CognitionProvenance {
    /// 本机账本写下的一版：来源节点与它的提交一并记录。
    pub fn local(node: impl Into<String>, commit: CommitRef) -> Self {
        Self {
            origin_node: node.into(),
            origin_commit: Some(commit),
            source_digest: None,
        }
    }

    /// 从一个节点读到的一版：原节点已知，提交与原件摘要可以缺席。
    pub fn imported(node: impl Into<String>) -> Self {
        Self {
            origin_node: node.into(),
            origin_commit: None,
            source_digest: None,
        }
    }

    pub fn with_source(mut self, digest: Digest) -> Self {
        self.source_digest = Some(digest);
        self
    }

    fn validate(&self) -> Result<(), ShenkoError> {
        if self.origin_node.trim().is_empty() {
            return Err(ShenkoError::conflict(
                "a sealed cognition record must name the node whose ledger wrote it",
            ));
        }
        if let Some(digest) = &self.source_digest {
            require_canonical_digest(digest, "provenance source")?;
        }
        Ok(())
    }
}

/// 一条态度或一份宏包声明适用的场合。
///
/// 全部缺席表示主体整体，不是「谁都可见」。它是来源元数据，不做访问过滤：主体自己
/// 的知识不因场合被藏起来（与 `contacts` 的 `ClaimScope` 同向）。分支根是 nil
/// UUID，是合法的根分支。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitionScope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<BranchId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<EntityId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<EntityId>,
}

impl CognitionScope {
    /// 适用于主体整体，而不是某一次对话或某个任务。
    pub fn subject_wide() -> Self {
        Self::default()
    }

    pub fn is_subject_wide(&self) -> bool {
        self.branch.is_none() && self.conversation.is_none() && self.task.is_none()
    }

    pub fn in_branch(mut self, branch: BranchId) -> Self {
        self.branch = Some(branch);
        self
    }

    pub fn in_conversation(mut self, conversation: EntityId) -> Self {
        self.conversation = Some(conversation);
        self
    }

    pub fn in_task(mut self, task: EntityId) -> Self {
        self.task = Some(task);
        self
    }

    fn validate(&self) -> Result<(), ShenkoError> {
        for (label, id) in [
            ("conversation", self.conversation),
            ("task", self.task),
        ] {
            if id.is_some_and(|id| id.as_uuid().is_nil()) {
                return Err(ShenkoError::conflict(format!(
                    "a cognition scope names a {label} identity; the nil id is not one",
                )));
            }
        }
        Ok(())
    }
}

/// 一条态度什么时候过期。
///
/// TTL 只说明这条判断的时效，不改内容字节，也不是遗忘：遗忘仍按已安装的宏包与
/// 投影状态记录。两种形式都绑定记录它的账本时钟；跨时钟域比较年龄就是猜。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttitudeTtl {
    /// 在记录它的那个账本时钟上到期。
    ExpiresAt { at_ms: i64, ledger_id: EntityId },
    /// 从记录时间起经过给定相对年龄后到期；求值必须落在同一个账本时钟。
    ElapsedAfter { after_ms: u64, ledger_id: EntityId },
}

impl AttitudeTtl {
    /// 这个 TTL 所属的账本时钟域。
    pub fn ledger_id(self) -> EntityId {
        match self {
            Self::ExpiresAt { ledger_id, .. } | Self::ElapsedAfter { ledger_id, .. } => ledger_id,
        }
    }

    /// 在当前账本时钟上求值。
    ///
    /// 时钟域不同、记录时间缺席、或时钟读得比记录还早时返回
    /// [`AttitudeFreshness::Unknown`]：那是不成立的年龄，不猜成新鲜，也不猜成过期。
    pub fn evaluate(self, recorded_at_ms: i64, clock: AttitudeClock) -> AttitudeFreshness {
        if self.ledger_id() != clock.ledger_id {
            return AttitudeFreshness::Unknown {
                detail: format!(
                    "ttl belongs to ledger {}; this evaluation uses ledger {}",
                    self.ledger_id(),
                    clock.ledger_id
                ),
            };
        }
        match self {
            Self::ExpiresAt { at_ms, .. } => {
                let delta = clock.now_ms as i128 - at_ms as i128;
                if delta >= 0 {
                    AttitudeFreshness::Stale {
                        expired_for_ms: delta as u64,
                    }
                } else {
                    AttitudeFreshness::Fresh {
                        remaining_ms: (-delta) as u64,
                    }
                }
            }
            Self::ElapsedAfter { after_ms, .. } => {
                if recorded_at_ms <= 0 {
                    return AttitudeFreshness::Unknown {
                        detail: "the record carries no usable recording time".to_string(),
                    };
                }
                if clock.now_ms < recorded_at_ms {
                    return AttitudeFreshness::Unknown {
                        detail: "the clock reads earlier than the record; age cannot be proven"
                            .to_string(),
                    };
                }
                let age = (clock.now_ms as i128 - recorded_at_ms as i128) as u64;
                if age >= after_ms {
                    AttitudeFreshness::Stale {
                        expired_for_ms: age - after_ms,
                    }
                } else {
                    AttitudeFreshness::Fresh {
                        remaining_ms: after_ms - age,
                    }
                }
            }
        }
    }
}

/// 求值 TTL 用的账本时钟。`ledger_id` 是时钟域，不是权限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttitudeClock {
    pub ledger_id: EntityId,
    pub now_ms: i64,
}

impl AttitudeClock {
    pub fn new(ledger_id: EntityId, now_ms: i64) -> Self {
        Self { ledger_id, now_ms }
    }
}

/// 一条态度在某个时钟上的时效读法。四者是不同的事实，不互相顶替。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AttitudeFreshness {
    /// 没有记录 TTL。这不是「仍然新鲜」的断言。
    NotRecorded,
    Fresh { remaining_ms: u64 },
    Stale { expired_for_ms: u64 },
    /// 无法在当前时钟域求值；绝不猜成 fresh 或 stale。
    Unknown { detail: String },
}

/// 一条态度的一等封存记录。
///
/// [`ContentAttitude`] 是本机账本读出来的一行（含本机提交与可能缺席的置信）。
/// `Attitude` 是同一判断可交付给另一个进程／节点的封存形态：同一身份、同一内容摘要、
/// 同一理由与置信，另带来源、适用场合与可选时效。两者描述同一个判断，不是第二次储存；
/// [`Attitude::from_ledger`] 是唯一的提升路径。
///
/// `schema_version` 是封存字节的一部分：其他版本由调用方显式迁移，这个读者不兼容
/// 未版本化或未来版本，也不把它当成空值默认掉。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Attitude {
    pub schema_version: u32,
    pub attitude_id: EntityId,
    pub agent_id: EntityId,
    pub digest: Digest,
    pub role: AttitudeRole,
    /// 理由指针。空表示没有记录理由，读作 `Unstated`。
    pub reasons: Vec<ReasonPointer>,
    /// `None` = 该记录没有携带置信；`Some(Unknown)` = 主体明确尚未判断。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<AttitudeConfidence>,
    /// 可学习的自由标签，只作提示，不是种类边界。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub knowledge_kind_hint: Option<String>,
    /// 这条态度取代的上一态度（不是内容）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<EntityId>,
    pub provenance: CognitionProvenance,
    #[serde(skip_serializing_if = "CognitionScope::is_subject_wide")]
    pub scope: CognitionScope,
    /// 这条判断的时效；`None` 表示没有记录 TTL，不表示永不过期。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<AttitudeTtl>,
    /// 写节点记录这条判断的机制时间（该节点时钟的 epoch 毫秒）。
    pub recorded_at_ms: i64,
}

impl Attitude {
    pub const SCHEMA_VERSION: u32 = ATTITUDE_SCHEMA_VERSION;

    /// 把本机账本的一行提升成封存记录。
    ///
    /// 本机提交进入来源；适用场合默认主体整体，时效默认未记录。这不是重写账本行：
    /// 原行仍由 Store 拥有。
    pub fn from_ledger(row: &ContentAttitude, origin_node: impl Into<String>) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            attitude_id: row.attitude_id,
            agent_id: row.agent_id,
            digest: row.digest.clone(),
            role: row.role.clone(),
            reasons: row.reasons.clone(),
            confidence: row.confidence,
            knowledge_kind_hint: row.knowledge_kind_hint.clone(),
            supersedes: row.supersedes,
            provenance: CognitionProvenance::local(origin_node, row.created_commit),
            scope: CognitionScope::subject_wide(),
            ttl: None,
            recorded_at_ms: row.created_at_ms,
        }
    }

    pub fn reading(&self) -> ReasonReading {
        ReasonPointer::reading(&self.reasons)
    }

    pub fn confidence_state(&self) -> ConfidenceState {
        ConfidenceState::of(self.confidence)
    }

    /// 这条判断在给定账本时钟上的时效。
    pub fn freshness(&self, clock: AttitudeClock) -> AttitudeFreshness {
        match self.ttl {
            Some(ttl) => ttl.evaluate(self.recorded_at_ms, clock),
            None => AttitudeFreshness::NotRecorded,
        }
    }

    /// 封存前的机械校验：只拒绝无法诚实交付的记录，不判断内容的意义。
    pub fn validate(&self) -> Result<(), ShenkoError> {
        if self.schema_version != Self::SCHEMA_VERSION {
            return Err(ShenkoError::conflict(format!(
                "attitude record schema version {} is not supported (this build admits {})",
                self.schema_version,
                Self::SCHEMA_VERSION
            )));
        }
        if self.attitude_id.as_uuid().is_nil() {
            return Err(ShenkoError::conflict(
                "a sealed attitude needs a non-nil attitude identity",
            ));
        }
        if self.agent_id.as_uuid().is_nil() {
            return Err(ShenkoError::conflict(
                "a sealed attitude needs a non-nil author identity",
            ));
        }
        require_canonical_digest(&self.digest, "attitude content")?;
        match &self.role {
            AttitudeRole::Adoption(target) => {
                if target.unit_id.trim().is_empty() {
                    return Err(ShenkoError::conflict("adoption requires a unit id"));
                }
                if target.unit_id.trim_start().starts_with("cap.") {
                    return Err(ShenkoError::conflict(
                        "adoption names a unit id; `cap.` names a capability contract (docs/specs/cognition-units.md#spec-51-8)",
                    ));
                }
                if target.member.as_deref().is_some_and(|member| member.trim().is_empty()) {
                    return Err(ShenkoError::conflict(
                        "an adoption member slot is either absent or nonempty",
                    ));
                }
            }
            AttitudeRole::Definition(slot) => {
                if slot.trim().is_empty() {
                    return Err(ShenkoError::conflict("a definition requires a slot name"));
                }
            }
            AttitudeRole::Revision { supersedes } => {
                require_canonical_digest(supersedes, "superseded content")?;
                if !self.reasons.iter().any(|reason| reason.stops_at(supersedes)) {
                    return Err(ShenkoError::conflict(
                        "a revision must carry a content reason pointing at the version it supersedes",
                    ));
                }
            }
            AttitudeRole::Assertion
            | AttitudeRole::Projection(_)
            | AttitudeRole::Unfinished
            | AttitudeRole::EffectUnknown => {}
        }
        for reason in &self.reasons {
            validate_reason(reason)?;
        }
        if let Some(AttitudeConfidence::Stated { value }) = self.confidence
            && !value.is_finite()
        {
            return Err(ShenkoError::conflict(
                "attitude confidence must be a finite value",
            ));
        }
        if let Some(hint) = &self.knowledge_kind_hint
            && hint.trim().is_empty()
        {
            return Err(ShenkoError::conflict(
                "a knowledge kind hint is either absent or nonempty",
            ));
        }
        self.provenance.validate()?;
        self.scope.validate()?;
        if let Some(ttl) = self.ttl {
            if ttl.ledger_id().as_uuid().is_nil() {
                return Err(ShenkoError::conflict(
                    "an attitude TTL must name the ledger clock it is recorded on",
                ));
            }
            if let Some(commit) = self.provenance.origin_commit
                && ttl.ledger_id() != commit.ledger_id
            {
                return Err(ShenkoError::conflict(
                    "an attitude TTL must use the same ledger clock as its origin commit",
                ));
            }
        }
        Ok(())
    }

    /// 校验后返回自身；拒绝的记录不进入封存路径。
    pub fn seal(self) -> Result<Self, ShenkoError> {
        self.validate()?;
        Ok(self)
    }

    /// 规范的封存字节：结构体字段顺序固定，JSON 对象顺序因此稳定。
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ShenkoError> {
        serde_json::to_vec(self).map_err(|error| ShenkoError::storage(error.to_string()))
    }

    /// 封存字节的内容身份。字节没变就是同一版。
    pub fn content_digest(&self) -> Result<Digest, ShenkoError> {
        Ok(Digest::blake3(&self.canonical_bytes()?))
    }
}

impl<'de> Deserialize<'de> for Attitude {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Repr {
            schema_version: u32,
            attitude_id: EntityId,
            agent_id: EntityId,
            digest: Digest,
            role: AttitudeRole,
            #[serde(default)]
            reasons: Vec<ReasonPointer>,
            #[serde(default)]
            confidence: Option<AttitudeConfidence>,
            #[serde(default)]
            knowledge_kind_hint: Option<String>,
            #[serde(default)]
            supersedes: Option<EntityId>,
            provenance: CognitionProvenance,
            #[serde(default)]
            scope: CognitionScope,
            #[serde(default)]
            ttl: Option<AttitudeTtl>,
            recorded_at_ms: i64,
        }
        let repr = Repr::deserialize(deserializer)?;
        if repr.schema_version != ATTITUDE_SCHEMA_VERSION {
            return Err(serde::de::Error::custom(format!(
                "unsupported attitude record schema version {} (this build admits {})",
                repr.schema_version, ATTITUDE_SCHEMA_VERSION
            )));
        }
        let record = Self {
            schema_version: repr.schema_version,
            attitude_id: repr.attitude_id,
            agent_id: repr.agent_id,
            digest: repr.digest,
            role: repr.role,
            reasons: repr.reasons,
            confidence: repr.confidence,
            knowledge_kind_hint: repr.knowledge_kind_hint,
            supersedes: repr.supersedes,
            provenance: repr.provenance,
            scope: repr.scope,
            ttl: repr.ttl,
            recorded_at_ms: repr.recorded_at_ms,
        };
        record.validate().map_err(serde::de::Error::custom)?;
        Ok(record)
    }
}

/// 宏包成员需要怎样被采纳。默认由成员自己的种类决定。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetapackageMemberAdoption {
    /// 成员自己的种类决定：内容需要一个已认证的采纳态度，代码只需安装。
    #[default]
    Required,
    /// 作者额外要求：即使是代码或子宏包成员也必须有采纳态度。
    Attested,
}

/// 一份宏包声明里的一个成员。
///
/// 成员用单元 id，不用能力合同（`cap.` 前缀属于另一套标识）。`contract` 是一个
/// 角色槽：成员必须自己声明提供这一合同，于是使用方按角色解析，而不是钉死某个包。
/// 这里的检查只核对形状；合同的精确解析仍由安装权威（Node 的 unit store）完成。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetapackageMember {
    pub unit_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_digest: Option<Digest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<String>,
    #[serde(default)]
    pub adoption: MetapackageMemberAdoption,
}

impl MetapackageMember {
    pub fn new(unit_id: impl Into<String>) -> Self {
        Self {
            unit_id: unit_id.into(),
            revision: None,
            artifact_digest: None,
            contract: None,
            adoption: MetapackageMemberAdoption::Required,
        }
    }

    fn validate(&self) -> Result<(), ShenkoError> {
        if self.unit_id.trim().is_empty() {
            return Err(ShenkoError::conflict(
                "a metapackage member needs a unit id",
            ));
        }
        if self.unit_id.trim_start().starts_with("cap.") {
            return Err(ShenkoError::conflict(
                "a metapackage member names a unit id; `cap.` names a capability contract (docs/specs/cognition-units.md#spec-51-8)",
            ));
        }
        if self.revision.as_deref().is_some_and(|revision| revision.trim().is_empty()) {
            return Err(ShenkoError::conflict(
                "a metapackage member revision requirement is either absent or nonempty",
            ));
        }
        if let Some(digest) = &self.artifact_digest {
            require_canonical_digest(digest, "metapackage member artifact")?;
        }
        if let Some(contract) = &self.contract {
            let name = contract.strip_prefix("cap.").unwrap_or("");
            if name.trim().is_empty() || contract != contract.trim() {
                return Err(ShenkoError::conflict(
                    "a metapackage member contract names `cap.<name>` and nothing else",
                ));
            }
        }
        Ok(())
    }
}

/// 一份宏包声明的一等封存记录。
///
/// 宏包本身就是一组被一起安装的单元：插件、awareness／perception 等内容单元与子宏包
/// 都是成员。`taste` 与 `humanlike-mind` 是并列的两份宏包，谁也不包含谁。
///
/// 这是声明与来源，不是安装决议：本机是否满足成员、选了哪个提供者、是否启用，仍由
/// Node 的 unit store 各自记录（「复制成功不是本地已解析」）。给宏包本身打分属于对它的
/// 采纳态度（[`Attitude`]），不塞进声明里冒充声明字段。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Metapackage {
    pub schema_version: u32,
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: Digest,
    pub members: Vec<MetapackageMember>,
    pub provenance: CognitionProvenance,
    #[serde(skip_serializing_if = "CognitionScope::is_subject_wide")]
    pub scope: CognitionScope,
}

impl Metapackage {
    pub const SCHEMA_VERSION: u32 = METAPACKAGE_SCHEMA_VERSION;

    pub fn member(&self, unit_id: &str) -> Option<&MetapackageMember> {
        self.members.iter().find(|member| member.unit_id == unit_id)
    }

    /// 封存前的机械校验：拒绝无法诚实交付的声明，不判断成员该不该装。
    pub fn validate(&self) -> Result<(), ShenkoError> {
        if self.schema_version != Self::SCHEMA_VERSION {
            return Err(ShenkoError::conflict(format!(
                "metapackage schema version {} is not supported (this build admits {})",
                self.schema_version,
                Self::SCHEMA_VERSION
            )));
        }
        if self.unit_id.trim().is_empty() {
            return Err(ShenkoError::conflict(
                "a metapackage needs a unit id",
            ));
        }
        if self.unit_id.trim_start().starts_with("cap.") {
            return Err(ShenkoError::conflict(
                "a metapackage names a unit id; `cap.` names a capability contract",
            ));
        }
        if self.revision.trim().is_empty() {
            return Err(ShenkoError::conflict(
                "a metapackage needs a revision",
            ));
        }
        require_canonical_digest(&self.artifact_digest, "metapackage artifact")?;
        let mut ids: Vec<&str> = Vec::new();
        for member in &self.members {
            member.validate()?;
            if ids.contains(&member.unit_id.as_str()) {
                return Err(ShenkoError::conflict(format!(
                    "metapackage {} declares member {:?} twice",
                    self.unit_id, member.unit_id
                )));
            }
            ids.push(member.unit_id.as_str());
        }
        self.provenance.validate()?;
        self.scope.validate()?;
        Ok(())
    }

    pub fn seal(self) -> Result<Self, ShenkoError> {
        self.validate()?;
        Ok(self)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ShenkoError> {
        serde_json::to_vec(self).map_err(|error| ShenkoError::storage(error.to_string()))
    }

    pub fn content_digest(&self) -> Result<Digest, ShenkoError> {
        Ok(Digest::blake3(&self.canonical_bytes()?))
    }
}

impl<'de> Deserialize<'de> for Metapackage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Repr {
            schema_version: u32,
            unit_id: String,
            revision: String,
            artifact_digest: Digest,
            #[serde(default)]
            members: Vec<MetapackageMember>,
            provenance: CognitionProvenance,
            #[serde(default)]
            scope: CognitionScope,
        }
        let repr = Repr::deserialize(deserializer)?;
        if repr.schema_version != METAPACKAGE_SCHEMA_VERSION {
            return Err(serde::de::Error::custom(format!(
                "unsupported metapackage schema version {} (this build admits {})",
                repr.schema_version, METAPACKAGE_SCHEMA_VERSION
            )));
        }
        let record = Self {
            schema_version: repr.schema_version,
            unit_id: repr.unit_id,
            revision: repr.revision,
            artifact_digest: repr.artifact_digest,
            members: repr.members,
            provenance: repr.provenance,
            scope: repr.scope,
        };
        record.validate().map_err(serde::de::Error::custom)?;
        Ok(record)
    }
}

/// 一条理由指针里每个摘要都必须是本 CAS 能解析的规范 blake3。
fn validate_reason(reason: &ReasonPointer) -> Result<(), ShenkoError> {
    if let Some(perception) = &reason.via_perception {
        require_canonical_digest(perception, "reason perception")?;
    }
    match &reason.termination {
        ReasonTermination::Observation(observation) => {
            if observation.event_id.as_uuid().is_nil() {
                return Err(ShenkoError::conflict(
                    "a reason stops at an observation; the nil id is not one",
                ));
            }
            if observation.receipt_id.is_some_and(|id| id.as_uuid().is_nil()) {
                return Err(ShenkoError::conflict(
                    "an observation receipt is either absent or a real identity",
                ));
            }
        }
        ReasonTermination::Source(source) => {
            require_canonical_digest(&source.digest, "reason source")?;
        }
        ReasonTermination::Receipt(receipt) => {
            if receipt.receipt_id.as_uuid().is_nil() {
                return Err(ShenkoError::conflict(
                    "a reason stops at a receipt; the nil id is not one",
                ));
            }
        }
        ReasonTermination::Reconciliation(case) => {
            if case.case_id.as_uuid().is_nil() {
                return Err(ShenkoError::conflict(
                    "a reason stops at a reconciliation case; the nil id is not one",
                ));
            }
        }
        ReasonTermination::Content(digest) => {
            require_canonical_digest(digest, "reason content")?;
        }
        ReasonTermination::Perspective(view) => {
            if view.subject.trim().is_empty() {
                return Err(ShenkoError::conflict(
                    "a perspective reason names whose perception it used",
                ));
            }
            if let Some(perception) = &view.perception {
                require_canonical_digest(perception, "perspective perception")?;
            }
            require_canonical_digest(&view.understanding, "perspective understanding")?;
        }
    }
    Ok(())
}

/// 只承认本 CAS 写得出、也安全解析得了的规范摘要。
fn require_canonical_digest(digest: &Digest, label: &str) -> Result<(), ShenkoError> {
    let canonical = digest.algorithm() == "blake3"
        && digest.hex().len() == 64
        && digest
            .hex()
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !canonical {
        return Err(ShenkoError::conflict(format!(
            "{label} digest {digest} is not a canonical blake3 digest"
        )));
    }
    Ok(())
}

/// 修订链上的一版。按账本顺序从新到旧。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRevisionEntry {
    pub digest: Digest,
    /// 记录这次修订的态度。链的起点（最早那版）也有登记它的那条态度。
    pub attitude_id: EntityId,
    pub created_at_ms: i64,
}

/// 统一的修订露出 。
///
/// 不特意查看时只露出这三项：旧版本数量、最新修改时间、最新一版的摘要；
/// 全文只在按版本显式读取时给出。投影规则本身写在已安装的 `taste` 宏包里，
/// 这里只给出事实。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRevisionDisclosure {
    pub latest_digest: Digest,
    pub old_version_count: u32,
    pub latest_modified_at_ms: i64,
}

/// 一条理由指针在本机的可核性。
///
/// 它只回答「本机账本／对象库是否真的有这条记录，而且与指针相符」，
/// **不回答**「它是否证明了这条内容」。外来或导入的回执如实记为缺席，
/// 不冒充本机回执，也不因此否定主体的学习。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonAdmission {
    /// 本机找到该记录／对象，且细节与指针相符。
    PresentLocal,
    /// 本机有同名记录，但细节不符（观察级别、长度、摘要）。
    DetailMismatch { detail: String },
    /// 本机没有该记录：来自别的节点、尚未执行或已经丢失。
    AbsentLocal { detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasonAdmissionReport {
    pub reason: ReasonPointer,
    pub admission: ReasonAdmission,
}

/// 采纳的证据状态。
///
/// 「已采纳」是账本事实；「证据已被本机核实」是另一件事，只能由真正拥有那套
/// 本地试验／观察验证的领域交出裁决。两者不得互相冒充。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdoptionEvidence {
    /// 拥有验证机制的领域交回：本机真实执行／观察过，并核实了这些理由。
    VerifiedLocal,
    /// 拥有验证机制的领域交回：本机核实失败。
    RefutedLocal,
    /// 未判定：本机没有核实（没有验证者、理由缺席，或理由来自别的节点）。
    /// 既不是已证实，也不是已否定。
    Undetermined { detail: String },
}

/// 一个单元对某一版内容的采纳投影。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentAdoption {
    pub attitude_id: EntityId,
    pub unit_id: String,
    pub content_digest: Digest,
    /// 采纳态度存在（账本事实）。与证据是否核实无关。
    pub adopted: bool,
    pub confidence: ConfidenceState,
    pub reasons: Vec<ReasonAdmissionReport>,
    pub evidence: AdoptionEvidence,
    pub recorded_commit: CommitRef,
    pub recorded_at_ms: i64,
}

/// 证据核实委派。
///
/// 只有拥有真实本地试验／观察验证的领域能回答 `Some(true/false)`；
/// 无法判定的返回 `None`，调用方保持 [`AdoptionEvidence::Undetermined`]。
/// 这是证据准入，不是人工审批，也不是模型打分。
pub trait AdoptionEvidenceVerifier: Send + Sync {
    fn verify_adoption(
        &self,
        agent_id: EntityId,
        digest: &Digest,
        reasons: &[ReasonPointer],
    ) -> Result<Option<bool>, ShenkoError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::LedgerSeq;
    use serde_json::json;

    fn digest(seed: &str) -> Digest {
        Digest::blake3(seed.as_bytes())
    }

    /// 理由停在原始一侧读作 fact；停在内容或他者视角上读作 conclusion。
    #[test]
    fn reading_follows_reason_termination() {
        let observed = ReasonPointer::observation(EntityId::new(), None);
        let quoted = ReasonPointer::source(digest("original"), None);
        let receipt = ReasonPointer::receipt(EntityId::new(), None);
        let case = ReasonPointer::reconciliation(EntityId::new());
        assert_eq!(ReasonPointer::reading(&[observed.clone()]), ReasonReading::Fact);
        assert_eq!(ReasonPointer::reading(&[quoted]), ReasonReading::Fact);
        assert_eq!(
            ReasonPointer::reading(&[observed.clone(), receipt, case]),
            ReasonReading::Fact
        );

        let derived = ReasonPointer::content(digest("earlier note"));
        assert_eq!(
            ReasonPointer::reading(&[observed.clone(), derived]),
            ReasonReading::Conclusion
        );
        let perspective =
            ReasonPointer::perspective("human:owner", None, digest("their reading"));
        assert_eq!(
            ReasonPointer::reading(&[perspective]),
            ReasonReading::Conclusion
        );
        assert_eq!(ReasonPointer::reading(&[]), ReasonReading::Unstated);
    }

    /// 未知、未记录与数值是三种状态，不互相顶替。
    #[test]
    fn confidence_states_never_collapse() {
        assert_eq!(ConfidenceState::of(None), ConfidenceState::Absent);
        assert_eq!(
            ConfidenceState::of(Some(AttitudeConfidence::unknown())),
            ConfidenceState::Unknown
        );
        assert_eq!(
            ConfidenceState::of(AttitudeConfidence::stated(0.0)),
            ConfidenceState::Stated { value: 0.0 }
        );
        // 内核不规定区间：1.5 只是一个主体给出的数，不被夹取也不被拒绝。
        assert_eq!(
            AttitudeConfidence::stated(1.5),
            Some(AttitudeConfidence::Stated { value: 1.5 })
        );
        assert_eq!(AttitudeConfidence::stated(f64::NAN), None);
        assert_eq!(AttitudeConfidence::stated(f64::INFINITY), None);
        assert_eq!(AttitudeConfidence::unknown().value(), None);
    }

    fn provenance(ledger: EntityId) -> CognitionProvenance {
        CognitionProvenance::local("node-a", CommitRef::new(ledger, LedgerSeq(3)))
    }

    fn sealed_attitude(ledger: EntityId) -> Attitude {
        let superseded = digest("content v1");
        Attitude {
            schema_version: ATTITUDE_SCHEMA_VERSION,
            attitude_id: EntityId::new(),
            agent_id: EntityId::new(),
            digest: digest("content v2"),
            role: AttitudeRole::Revision {
                supersedes: superseded.clone(),
            },
            reasons: vec![
                ReasonPointer::content(superseded),
                ReasonPointer::source(digest("original"), None),
            ],
            confidence: Some(AttitudeConfidence::stated(0.5).expect("finite")),
            knowledge_kind_hint: Some("fact".to_string()),
            supersedes: Some(EntityId::new()),
            provenance: provenance(ledger),
            scope: CognitionScope::subject_wide(),
            ttl: Some(AttitudeTtl::ExpiresAt {
                at_ms: 10_000,
                ledger_id: ledger,
            }),
            recorded_at_ms: 9_000,
        }
    }

    fn sealed_metapackage(ledger: EntityId) -> Metapackage {
        Metapackage {
            schema_version: METAPACKAGE_SCHEMA_VERSION,
            unit_id: "humanlike-mind".to_string(),
            revision: "1".to_string(),
            artifact_digest: digest("humanlike-mind package"),
            members: vec![
                MetapackageMember::new("forgetting"),
                MetapackageMember {
                    contract: Some("cap.forgetting".to_string()),
                    ..MetapackageMember::new("forgetting-host")
                },
            ],
            provenance: provenance(ledger),
            scope: CognitionScope::subject_wide(),
        }
    }

    /// 封存记录原样往返：字节身份、理由读法与置信状态都不因序列化改变。
    #[test]
    fn sealed_attitude_roundtrips_with_identity() {
        let ledger = EntityId::new();
        let record = sealed_attitude(ledger);
        record.validate().expect("a valid record seals");
        let bytes = record.canonical_bytes().expect("canonical bytes");
        let text = String::from_utf8(bytes.clone()).expect("utf8");
        assert!(text.contains("\"schema_version\":1"));

        let read: Attitude = serde_json::from_slice(&bytes).expect("admitted");
        assert_eq!(read, record);
        assert_eq!(
            read.content_digest().expect("digest"),
            record.content_digest().expect("digest")
        );
        assert_eq!(read.reading(), ReasonReading::Conclusion);
        assert_eq!(
            read.confidence_state(),
            ConfidenceState::Stated { value: 0.5 }
        );
    }

    /// 只准入这一版：未来版本、缺席版本、未知字段和坏摘要都不被读成默认值。
    #[test]
    fn sealed_attitude_admits_only_its_own_schema_version() {
        let ledger = EntityId::new();
        let record = sealed_attitude(ledger);
        let value = serde_json::to_value(&record).expect("json");

        let mut future = value.clone();
        future["schema_version"] = json!(ATTITUDE_SCHEMA_VERSION + 1);
        let error = serde_json::from_value::<Attitude>(future)
            .expect_err("a future version is not this record")
            .to_string();
        assert!(error.contains("schema version"), "{error}");

        let mut missing = value.clone();
        missing
            .as_object_mut()
            .expect("object")
            .remove("schema_version");
        assert!(serde_json::from_value::<Attitude>(missing).is_err());

        let mut unknown = value.clone();
        unknown["extra"] = json!(true);
        assert!(serde_json::from_value::<Attitude>(unknown).is_err());

        let mut broken = value;
        broken["digest"] = json!("not-a-digest");
        assert!(serde_json::from_value::<Attitude>(broken).is_err());
    }

    /// 本机账本行提升为封存记录时保留同一判断，不另开一版。
    #[test]
    fn ledger_row_promotes_to_the_same_judgement() {
        let ledger = EntityId::new();
        let row = ContentAttitude {
            attitude_id: EntityId::new(),
            agent_id: EntityId::new(),
            digest: digest("content"),
            role: AttitudeRole::Assertion,
            reasons: vec![ReasonPointer::observation(EntityId::new(), None)],
            confidence: Some(AttitudeConfidence::unknown()),
            knowledge_kind_hint: None,
            supersedes: None,
            created_commit: CommitRef::new(ledger, LedgerSeq(11)),
            created_at_ms: 42,
        };
        let record = Attitude::from_ledger(&row, "node-a");
        record.validate().expect("a ledger row seals");
        assert_eq!(record.attitude_id, row.attitude_id);
        assert_eq!(record.digest, row.digest);
        assert_eq!(record.recorded_at_ms, 42);
        assert_eq!(record.reading(), ReasonReading::Fact);
        assert_eq!(record.confidence_state(), ConfidenceState::Unknown);
        assert_eq!(
            record.provenance.origin_commit,
            Some(CommitRef::new(ledger, LedgerSeq(11)))
        );
    }

    /// TTL 只在同一账本时钟域求值；无法证明年龄时是 Unknown，不是新鲜。
    #[test]
    fn ttl_freshness_never_invents_an_age() {
        let ledger = EntityId::new();
        let expires = AttitudeTtl::ExpiresAt {
            at_ms: 1_000,
            ledger_id: ledger,
        };
        assert_eq!(
            expires.evaluate(0, AttitudeClock::new(ledger, 999)),
            AttitudeFreshness::Fresh { remaining_ms: 1 }
        );
        assert_eq!(
            expires.evaluate(0, AttitudeClock::new(ledger, 1_000)),
            AttitudeFreshness::Stale { expired_for_ms: 0 }
        );
        assert!(matches!(
            expires.evaluate(0, AttitudeClock::new(EntityId::new(), 1_000)),
            AttitudeFreshness::Unknown { .. }
        ));

        let relative = AttitudeTtl::ElapsedAfter {
            after_ms: 100,
            ledger_id: ledger,
        };
        assert!(matches!(
            relative.evaluate(0, AttitudeClock::new(ledger, 500)),
            AttitudeFreshness::Unknown { .. }
        ));
        assert_eq!(
            relative.evaluate(450, AttitudeClock::new(ledger, 500)),
            AttitudeFreshness::Fresh { remaining_ms: 50 }
        );
        assert_eq!(
            relative.evaluate(400, AttitudeClock::new(ledger, 500)),
            AttitudeFreshness::Stale { expired_for_ms: 0 }
        );
        assert!(matches!(
            relative.evaluate(600, AttitudeClock::new(ledger, 500)),
            AttitudeFreshness::Unknown { .. }
        ));

        let mut record = sealed_attitude(ledger);
        record.ttl = None;
        assert_eq!(
            record.freshness(AttitudeClock::new(ledger, 10_000)),
            AttitudeFreshness::NotRecorded
        );
    }

    /// 封存校验拒绝无法诚实交付的形状，而不是让人读成已判断。
    #[test]
    fn sealed_attitude_refuses_untruthful_shapes() {
        let ledger = EntityId::new();

        let mut record = sealed_attitude(ledger);
        record.attitude_id = EntityId::nil();
        assert!(record.validate().is_err());

        let mut record = sealed_attitude(ledger);
        record.role = AttitudeRole::Adoption(AdoptionTarget::unit("cap.forgetting"));
        assert!(record.validate().is_err());

        let mut record = sealed_attitude(ledger);
        record.reasons = vec![ReasonPointer::observation(EntityId::new(), None)];
        assert!(record.validate().is_err());

        let mut record = sealed_attitude(ledger);
        record.ttl = Some(AttitudeTtl::ExpiresAt {
            at_ms: 1,
            ledger_id: EntityId::new(),
        });
        assert!(record.validate().is_err());

        let mut record = sealed_attitude(ledger);
        record.provenance.origin_node = "  ".to_string();
        assert!(record.validate().is_err());
    }

    /// 宏包声明同样原样往返，并只准入自己的版本。
    #[test]
    fn sealed_metapackage_roundtrips_and_admits_only_its_version() {
        let ledger = EntityId::new();
        let record = sealed_metapackage(ledger);
        record.validate().expect("a valid declaration seals");
        let bytes = record.canonical_bytes().expect("canonical bytes");
        let read: Metapackage = serde_json::from_slice(&bytes).expect("admitted");
        assert_eq!(read, record);
        assert_eq!(
            read.content_digest().expect("digest"),
            record.content_digest().expect("digest")
        );
        assert_eq!(
            read.member("forgetting").map(|member| member.unit_id.as_str()),
            Some("forgetting")
        );

        let mut value = serde_json::to_value(&record).expect("json");
        value["schema_version"] = json!(METAPACKAGE_SCHEMA_VERSION + 1);
        assert!(serde_json::from_value::<Metapackage>(value).is_err());
    }

    /// 成员是单元 id：`cap.` 不混用，重复成员与空合同都被拒绝。
    #[test]
    fn metapackage_refuses_identity_confusion_and_duplicates() {
        let ledger = EntityId::new();

        let mut record = sealed_metapackage(ledger);
        record.members.push(MetapackageMember::new("cap.forgetting"));
        assert!(record.validate().is_err());

        let mut record = sealed_metapackage(ledger);
        record.members.push(MetapackageMember::new("forgetting"));
        assert!(record.validate().is_err());

        let mut record = sealed_metapackage(ledger);
        record.unit_id = "cap.taste".to_string();
        assert!(record.validate().is_err());

        let mut member = MetapackageMember::new("forgetting-host");
        member.contract = Some("forgetting".to_string());
        assert!(member.validate().is_err());
    }
}
