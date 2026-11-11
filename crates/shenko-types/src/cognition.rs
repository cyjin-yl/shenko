//! 认知原子：不可变内容、账本上的态度、理由指针 — docs/specs/cognition-units.md#spec-51-1, docs/specs/cognition-units.md#spec-51-3, docs/specs/cognition-units.md#spec-51-4, docs/specs/cognition-units.md#spec-51-9
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
    /// 账本写入这版内容的机制时间。不是时间感知能力（docs/specs/cognition-units.md#spec-51-3）。
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
/// 不填默认数字，也不另造量表（docs/specs/cognition-units.md#spec-51-1）。
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
    /// 已有领域的合同，不属于内核新造的量表（docs/specs/cognition-units.md#spec-51-1）。
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

/// 一次观察事件。观察属于原始经历，不是 perception 单元本身（docs/specs/cognition-units.md#spec-51-2）。
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

/// 证据回执（docs/specs/durable-state.md#spec-41-4 X04）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptRef {
    pub receipt_id: EntityId,
    #[serde(default)]
    pub observation_level: Option<ObservationLevel>,
}

/// 对账案例（docs/specs/durable-state.md#spec-41-4）。外部效应的未知状态仍是它自己的机制记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciliationRef {
    pub case_id: EntityId,
}

/// 经由他者 perception 的理解。理由停在这份理解上，不停在自己的观察上，
/// 所以它读起来是「推测对方会这样看」，不是自己的 fact（docs/specs/cognition-units.md#spec-51-2）。
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

    /// 一组理由停在哪里的读法（docs/specs/cognition-units.md#spec-51-1）。
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

/// 采纳目标：内容作为哪个单元的成员被接纳（docs/specs/cognition-units.md#spec-51-1, docs/specs/cognition-units.md#spec-51-8）。
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

/// 遗忘／投影的六个阶段（docs/specs/cognition-units.md#spec-51-7）。状态不改字节，只改默认露出。
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

/// 账本上的一条态度。supersedes 只追加，不覆盖旧行（docs/specs/cognition-units.md#spec-51-1, docs/specs/cognition-units.md#spec-51-3）。
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

/// 修订链上的一版。按账本顺序从新到旧。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRevisionEntry {
    pub digest: Digest,
    /// 记录这次修订的态度。链的起点（最早那版）也有登记它的那条态度。
    pub attitude_id: EntityId,
    pub created_at_ms: i64,
}

/// 统一的修订露出（docs/specs/cognition-units.md#spec-51-1, docs/specs/cognition-units.md#spec-51-4）。
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
}
