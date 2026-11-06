//! 认知原子：不可变内容、账本上的态度、理由指针 — SPEC §51.1, §51.3, §51.4, §51.9
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

use serde::{Deserialize, Serialize};

use crate::commit::CommitRef;
use crate::error::ShenkoError;
use crate::evidence::ObservationLevel;
use crate::ids::EntityId;
use crate::object::{Digest, ObjectRef};

/// 一版已经登记的内容。字节在 CAS 里不可变，`object.digest` 就是它的身份。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRecord {
    pub object: ObjectRef,
    /// 账本写入这版内容的机制时间。不是时间感知能力（SPEC §51.3）。
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
/// 不填默认数字，也不另造量表（SPEC §51.1）。
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

/// 一次观察事件。观察属于原始经历，不是 perception 单元本身（SPEC §51.2）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationRef {
    pub event_id: EntityId,
    #[serde(default)]
    pub receipt_id: Option<EntityId>,
}

/// 原文／原件：不可变的原始字节，按摘要寻址。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    pub digest: Digest,
    #[serde(default)]
    pub locator: Option<String>,
}

/// 证据回执（SPEC §41.4 X04）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptRef {
    pub receipt_id: EntityId,
    #[serde(default)]
    pub observation_level: Option<ObservationLevel>,
}

/// 对账案例（SPEC §41.4）。外部效应的未知状态仍是它自己的机制记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciliationRef {
    pub case_id: EntityId,
}

/// 经由他者 perception 的理解。理由停在这份理解上，不停在自己的观察上，
/// 所以它读起来是「推测对方会这样看」，不是自己的 fact（SPEC §51.2）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerspectiveRef {
    pub subject: String,
    #[serde(default)]
    pub perception: Option<Digest>,
    pub understanding: Digest,
}

/// 理由停在哪里。这是读法，不是内容的 kind。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonTermination {
    Observation(ObservationRef),
    Source(SourceRef),
    Receipt(ReceiptRef),
    Reconciliation(ReconciliationRef),
    Content(Digest),
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

    /// 一组理由停在哪里的读法（SPEC §51.1）。
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
    Unstated,
}

/// 采纳目标：内容作为哪个单元的成员被接纳（SPEC §51.1, §51.8）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdoptionTarget {
    pub unit_id: String,
    #[serde(default)]
    pub unit_version: Option<String>,
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

/// 遗忘／投影的六个阶段（SPEC §51.7）。状态不改字节，只改默认露出。
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

/// 态度在账本上承担的机制角色。这不是内容的语义 kind。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttitudeRole {
    Assertion,
    Adoption(AdoptionTarget),
    Revision { supersedes: Digest },
    Definition(String),
    Projection(ProjectionStage),
    Unfinished,
    EffectUnknown,
}

impl AttitudeRole {
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
}

/// 账本上的一条态度。supersedes 只追加，不覆盖旧行（SPEC §51.1, §51.3）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentAttitude {
    pub attitude_id: EntityId,
    pub agent_id: EntityId,
    pub digest: Digest,
    pub role: AttitudeRole,
    pub reasons: Vec<ReasonPointer>,
    pub confidence: Option<AttitudeConfidence>,
    #[serde(default)]
    pub knowledge_kind_hint: Option<String>,
    #[serde(default)]
    pub supersedes: Option<EntityId>,
    pub created_commit: CommitRef,
    pub created_at_ms: i64,
}

impl ContentAttitude {
    pub fn confidence_state(&self) -> ConfidenceState {
        ConfidenceState::of(self.confidence)
    }
}

/// 修订链上的一版。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRevisionEntry {
    pub digest: Digest,
    pub attitude_id: EntityId,
    pub created_at_ms: i64,
}

/// 统一的修订露出（SPEC §51.1, §51.4）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRevisionDisclosure {
    pub latest_digest: Digest,
    pub old_version_count: u64,
    pub latest_modified_at_ms: i64,
}
