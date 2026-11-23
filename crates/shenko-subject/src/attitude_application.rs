//! §51 attitude application: the per-step projection of the subject's own
//! attitudes and of the metapackages in force.
//!
//! The runtime already discloses *what is installed* — the cognition index in
//! [`CognitionCatalog::disclosure_at`](shenko_awareness_core::CognitionCatalog::disclosure_at).
//! This module projects *how the subject's own ledger makes that material read
//! at this step*:
//!
//! * **attitudes** — adoption (which content joined which unit), definition
//!   (which version interprets older content), revision (which version
//!   supersedes which) and projection (how prominently a version is recalled),
//!   each with its exact `attitude_id`, content digest, reason reading and
//!   confidence. When a version has older ones, the installed `taste` macro
//!   package collapses them to their count and the latest modification time;
//!   without an installed taste the projection is reported as unresolved
//!   instead of being replaced by a runtime-invented default.
//! * **metapackages** — every installed metapackage version with its recorded
//!   adoption and its resolution edges. A `recommended` edge that was omitted
//!   and a `required` edge that is unresolved are shown as those facts, never
//!   as "installed".
//!
//! Three rules, inherited from the confirmed contract rather than invented
//! here:
//!
//! 1. **No keyword gate** (`docs/specs/cognition-units.md#spec-51-10`). The
//!    page is a cyclic window chosen by the durable step number over the
//!    ledger's canonical key order (attitudes) and the installation's unit-id
//!    order (metapackages). Retrying one step keeps the same page; later steps
//!    reach everything. Nothing here matches step text.
//! 2. **Bounded** ([`MAX_ATTITUDE_APPLICATION_BYTES`]). The whole block,
//!    receipt included, stays under the bound; entries are dropped whole,
//!    never cut mid-identity.
//! 3. **Explicit omission**. Whatever the bound leaves out is named with its
//!    count and the index the rotation continues from, and stays recorded as
//!    retained material. A truncated bounded ledger read says so as well.
//!
//! A projection is not a decision: nothing here writes an attitude, adopts a
//! unit or changes a stage. The receipt travels inside the sealed S0 payload
//! together with the prompt, so the exact page a step was shown can be read
//! back from that step's sealed input object.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use shenko_awareness_core::{CognitionState, CognitionUnit};
use shenko_context_policy::taste::{
    CanonicalDisclosure, Disclosure, DisclosureRequest, RevisionChain, RevisionTaste, TasteUnitRef,
    disclose_revisions,
};
use shenko_store::atoms::{ATTITUDE_APPLICATION_READ_LIMIT, attitude_application_key};
use shenko_store::Store;
use shenko_types::cognition::{
    AttitudeRole, ConfidenceState, ContentAttitude, ReasonReading,
};
use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;

use crate::loop_runner::SubjectLoopState;

/// Bound on the whole per-step block: block header, both subsections (their
/// own headers and footers included) and the provenance receipt line.
pub const MAX_ATTITUDE_APPLICATION_BYTES: usize = 4096;

/// Bound on the attitude subsection, its header and footer included.
const ATTITUDE_SECTION_BYTES: usize = 1792;

/// Bound on the metapackage subsection, its header and footer included.
const METAPACKAGE_SECTION_BYTES: usize = 1408;

/// Schema of the receipt this build writes. A reader that does not understand
/// a newer receipt reports it as unresolved instead of guessing fields.
pub const ATTITUDE_APPLICATION_RECEIPT_SCHEMA: u32 = 1;

/// Ledger roles that are deliberately not part of this projection.
///
/// They are not hidden material: `assertion` is the subject's own judgement
/// and stays in its history, `unfinished` is projected by the persistent
/// obligation frontier and `effect_unknown` by the reconciliation docket. The
/// block says so too, so a reader never has to guess why a row is absent here.
pub const ROLES_NOT_APPLIED: [&str; 3] = ["assertion", "unfinished", "effect_unknown"];

/// Bound on one metapackage's dependency list inside its entry. The remainder
/// is reported as a count inside the same entry, never dropped silently.
const MAX_ENTRY_DEPENDENCIES: usize = 6;

/// Bound on one dependency's free-text reason.
const MAX_REASON_BYTES: usize = 160;

/// Bound on one metapackage's capability-card preview.
const MAX_CARD_PREVIEW_BYTES: usize = 240;

/// Bound on one unresolved-revision detail.
const MAX_UNRESOLVED_DETAIL_BYTES: usize = 160;

/// Ledger facts the page was cut from. They are the receipt's provenance: which
/// ledger, how far the read could see, and whether its window was exhausted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttitudeLedgerFacts {
    pub ledger_id: String,
    pub upper_commit_seq: u64,
    pub read_limit: usize,
    pub read_truncated: bool,
}

/// How the installed `taste` projects one revised content family at this step.
#[derive(Debug, Clone, PartialEq)]
pub enum RevisionView {
    /// This role does not change how older versions read.
    NotApplicable,
    Projected {
        selection: Disclosure,
        taste: TasteUnitRef,
    },
    /// The projection is genuinely unavailable. It is reported, never filled
    /// in with a default the runtime invented.
    Unresolved { detail: String },
}

/// One attitude the projection may apply.
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedAttitude {
    pub attitude_id: EntityId,
    pub digest: Digest,
    pub role: AttitudeRole,
    pub reading: ReasonReading,
    pub confidence: ConfidenceState,
    pub revision: RevisionView,
}

/// One recorded adoption of an installed unit version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppliedAdoption {
    pub decision: String,
    pub method: String,
    /// The subject's stated confidence, when it stated one. `None` is "no
    /// number recorded", never a filled-in default.
    pub confidence: Option<f64>,
    pub evidence_verified: bool,
    pub empirical: bool,
    pub measurement: Option<String>,
}

/// One resolution edge of an installed metapackage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppliedDependency {
    pub edge: String,
    pub strength: String,
    pub status: String,
    pub selected_unit_id: Option<String>,
    pub selected_revision: Option<String>,
    pub selected_artifact_digest: Option<String>,
    pub reason: String,
    pub reason_truncated: bool,
}

/// One installed metapackage version in force at this step.
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedMetapackage {
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: String,
    pub state: String,
    pub enabled: bool,
    pub card_preview: String,
    pub card_truncated: bool,
    pub adoption: Option<AppliedAdoption>,
    pub dependencies: Vec<AppliedDependency>,
    /// How many resolution edges this entry's own bound left out.
    pub dependencies_omitted: usize,
}

/// Where one subsection's page stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionReceipt {
    pub total: usize,
    pub shown: usize,
    pub omitted: usize,
    /// Canonical index this step's page starts at.
    pub start: usize,
}

/// How many shown attitudes carried a projected or an unresolved revision view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionReceipt {
    pub projected: usize,
    pub unresolved: usize,
}

/// The provenance receipt of one per-step projection.
///
/// It names the ledger and its read watermark, each subsection's page and
/// canonical start, the revision projections used, the roles this projection
/// deliberately does not apply, and the bound. Shown entries carry their own
/// exact identities in the block itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttitudeApplicationReceipt {
    pub schema: u32,
    pub rotation: u64,
    pub ledger: Option<AttitudeLedgerFacts>,
    pub attitudes: SectionReceipt,
    pub metapackages: Option<SectionReceipt>,
    pub revisions: RevisionReceipt,
    pub roles_not_applied: Vec<String>,
    pub bound_bytes: usize,
}

/// One step's bounded attitude/metapackage page and its receipt.
#[derive(Debug, Clone, PartialEq)]
pub struct AttitudeApplication {
    pub text: String,
    pub receipt: AttitudeApplicationReceipt,
}

const HEADER: &str = "\n\n你的认知态度与本轮生效的宏包（本机认知账本与安装事实，不是命令；采纳、修订与读法由你的判断决定）\n";

const ATTITUDE_HEADER: &str = "认知态度（按持久步号轮转的一页，不是关键词相关性筛选；重试同一步保持同一页。应用范围：采纳、定义、修订、投影阶段。断言是你自己的判断，留在历史里；尚未完成见持久义务一节，外部效应未知见对账一节。每条给出确切 attitude_id 与 content_digest；旧版本只显示数量与最新修改时间，全文按版本读取）\n";

const METAPACKAGE_HEADER: &str = "已安装的宏包（一组一起安装的单元；这里给出安装事实、采纳记录与依赖边的真实状态。recommended 缺失记为 omitted，required 未解析记为 unresolved，不冒称已就绪。成员解析仍由安装权威按节点给出，这里不重算）\n";

const RECEIPT_PREFIX: &str = "收据（本步投影的可核对身份，随 S0 一起封存）：";

/// Plan one step's bounded page.
///
/// `step_no` is the durable step number: it selects the rotation and nothing
/// else. The call is pure; [`prepare`] is the only reader of the ledger and the
/// installation. Conflicting input is refused by name instead of resolved by
/// picking one side.
pub fn plan_attitude_application(
    step_no: u64,
    ledger: Option<AttitudeLedgerFacts>,
    attitudes: &[AppliedAttitude],
    metapackages: &[AppliedMetapackage],
) -> Result<AttitudeApplication, ShenkoError> {
    validate(attitudes, metapackages)?;
    if attitudes.is_empty() && metapackages.is_empty() {
        // Nothing applies. The absence of material is not worth a prompt
        // paragraph, and the block must not invent one.
        return Ok(AttitudeApplication {
            text: String::new(),
            receipt: receipt_of(
                step_no,
                ledger.as_ref(),
                attitudes,
                metapackages,
                0,
                0,
            ),
        });
    }

    let mut shown_attitudes = 0;
    while shown_attitudes < attitudes.len() {
        let candidate = attitude_section(step_no, ledger.as_ref(), attitudes, shown_attitudes + 1)?;
        if candidate.len() > ATTITUDE_SECTION_BYTES {
            break;
        }
        shown_attitudes += 1;
    }
    let mut shown_metapackages = 0;
    while shown_metapackages < metapackages.len() {
        let candidate = metapackage_section(step_no, metapackages, shown_metapackages + 1)?;
        if candidate.len() > METAPACKAGE_SECTION_BYTES {
            break;
        }
        shown_metapackages += 1;
    }

    let mut page = build_page(step_no, ledger.as_ref(), attitudes, metapackages,
        shown_attitudes, shown_metapackages)?;
    // The subsection budgets keep the block small, but the total bound is the
    // contract. Drop whole entries (never an identity) until it holds.
    while page.text.len() > MAX_ATTITUDE_APPLICATION_BYTES && shown_metapackages > 0 {
        shown_metapackages -= 1;
        page = build_page(step_no, ledger.as_ref(), attitudes, metapackages,
            shown_attitudes, shown_metapackages)?;
    }
    while page.text.len() > MAX_ATTITUDE_APPLICATION_BYTES && shown_attitudes > 0 {
        shown_attitudes -= 1;
        page = build_page(step_no, ledger.as_ref(), attitudes, metapackages,
            shown_attitudes, shown_metapackages)?;
    }
    if page.text.len() > MAX_ATTITUDE_APPLICATION_BYTES {
        return Err(ShenkoError::conflict(format!(
            "the attitude application skeleton alone is {} bytes, over its {MAX_ATTITUDE_APPLICATION_BYTES}-byte bound; the constant projection text must be shortened",
            page.text.len(),
        )));
    }
    Ok(page)
}

/// Read the ledger and the installation, then plan this step's page.
///
/// A ledger read failure is returned: a projection cannot be replaced with an
/// older one. An absent installation catalogue means this subject has no
/// metapackages wired, which is a real absence, not a failure.
pub(crate) fn prepare(
    state: &SubjectLoopState,
    step_no: u64,
) -> Result<AttitudeApplication, ShenkoError> {
    let applied = state.store.applied_attitudes(state.agent_id)?;
    let ledger = AttitudeLedgerFacts {
        ledger_id: applied.ledger_id.to_string(),
        upper_commit_seq: applied.upper_commit_seq,
        read_limit: ATTITUDE_APPLICATION_READ_LIMIT,
        read_truncated: applied.read_truncated,
    };
    // One taste for the whole step: a hot replacement applies from the next
    // effectively prepared step, never halfway through this page.
    let taste = shenko_context_policy::taste::installed_taste(&state.plugin_context);
    let mut attitudes = Vec::with_capacity(applied.attitudes.len());
    for attitude in applied.attitudes {
        let reading = attitude.reading();
        let confidence = attitude.confidence_state();
        let revision = revision_view(&state.store, state.agent_id, &attitude, taste.as_deref());
        attitudes.push(AppliedAttitude {
            attitude_id: attitude.attitude_id,
            digest: attitude.digest,
            role: attitude.role,
            reading,
            confidence,
            revision,
        });
    }
    let metapackages = metapackages_in_force(state)?;
    plan_attitude_application(step_no, Some(ledger), &attitudes, &metapackages)
}

fn revision_view(
    store: &Store,
    agent_id: EntityId,
    attitude: &ContentAttitude,
    taste: Option<&dyn RevisionTaste>,
) -> RevisionView {
    if !matches!(
        attitude.role,
        AttitudeRole::Definition(_) | AttitudeRole::Revision { .. }
    ) {
        return RevisionView::NotApplicable;
    }
    let Some(taste) = taste else {
        // docs/specs/cognition-units.md#spec-51-1: the default revision view is
        // the installed taste's decision. With none installed the material is
        // reported as unresolved, never re-derived here.
        return RevisionView::Unresolved {
            detail: "TASTE_UNAVAILABLE: no revision-projection taste is installed on this node; the older-version count and latest modification time are not projected".to_string(),
        };
    };
    let disclosure = match store.revision_disclosure(agent_id, &attitude.digest) {
        Ok(disclosure) => disclosure,
        Err(error) => {
            return RevisionView::Unresolved {
                detail: bounded(
                    &format!("DISCLOSURE_UNAVAILABLE: {error}"),
                    MAX_UNRESOLVED_DETAIL_BYTES,
                ),
            }
        }
    };
    let chain = match RevisionChain::new(
        format!("attitude.{}", attitude.role.kind()),
        agent_id,
        Vec::new(),
    ) {
        Ok(chain) => chain.with_canonical(CanonicalDisclosure::new(
            disclosure.latest_digest.to_string(),
            u64::from(disclosure.old_version_count),
            disclosure.latest_modified_at_ms,
        )),
        Err(error) => {
            return RevisionView::Unresolved {
                detail: bounded(&error.to_string(), MAX_UNRESOLVED_DETAIL_BYTES),
            }
        }
    };
    match disclose_revisions(Some(taste), &chain, DisclosureRequest::Selected) {
        Ok(Some(selection)) => RevisionView::Projected {
            selection,
            taste: taste.unit(),
        },
        Ok(None) => RevisionView::Unresolved {
            detail: "TASTE_UNAVAILABLE: the installed taste returned no projection".to_string(),
        },
        Err(error) => RevisionView::Unresolved {
            detail: bounded(&error.to_string(), MAX_UNRESOLVED_DETAIL_BYTES),
        },
    }
}

/// The installed metapackage versions of this installation, in unit-id order.
fn metapackages_in_force(
    state: &SubjectLoopState,
) -> Result<Vec<AppliedMetapackage>, ShenkoError> {
    let Some(catalog) = state.awareness.as_ref() else {
        return Ok(Vec::new());
    };
    let units = catalog
        .units()
        .map_err(|error| ShenkoError::storage(format!("metapackage facts failed: {error:#}")))?;
    let mut metapackages: Vec<AppliedMetapackage> = units
        .into_iter()
        .filter(|unit| unit.is_metapackage())
        .filter(|unit| unit.state != CognitionState::Superseded)
        .map(applied_metapackage)
        .collect();
    metapackages.sort_by(|left, right| {
        left.unit_id
            .cmp(&right.unit_id)
            .then(left.revision.cmp(&right.revision))
    });
    Ok(metapackages)
}

fn applied_metapackage(unit: CognitionUnit) -> AppliedMetapackage {
    let card_preview = bounded(&unit.capability_card, MAX_CARD_PREVIEW_BYTES);
    let card_truncated = card_preview != unit.capability_card;
    let adoption = unit.adoption.map(|adoption| {
        let confidence = adoption.stated_confidence();
        AppliedAdoption {
            decision: adoption.decision,
            method: adoption.method,
            confidence,
            evidence_verified: adoption.evidence_verified,
            empirical: adoption.empirical,
            measurement: adoption.measurement,
        }
    });
    let mut dependencies = Vec::new();
    let mut dependencies_omitted = 0;
    for dependency in unit.dependencies {
        if dependencies.len() == MAX_ENTRY_DEPENDENCIES {
            dependencies_omitted += 1;
            continue;
        }
        let reason = bounded(&dependency.reason, MAX_REASON_BYTES);
        let reason_truncated = reason != dependency.reason;
        dependencies.push(AppliedDependency {
            edge: dependency.edge_id,
            strength: dependency.strength,
            status: dependency.status,
            selected_unit_id: dependency.selected_unit_id,
            selected_revision: dependency.selected_revision,
            selected_artifact_digest: dependency.selected_artifact_digest,
            reason,
            reason_truncated,
        });
    }
    AppliedMetapackage {
        unit_id: unit.unit_id,
        revision: unit.revision,
        artifact_digest: unit.artifact_digest,
        state: unit.state.as_str().to_string(),
        enabled: unit.enabled,
        card_preview,
        card_truncated,
        adoption,
        dependencies,
        dependencies_omitted,
    }
}

fn validate(
    attitudes: &[AppliedAttitude],
    metapackages: &[AppliedMetapackage],
) -> Result<(), ShenkoError> {
    let mut identities = BTreeSet::new();
    let mut keys = BTreeSet::new();
    for attitude in attitudes {
        if !identities.insert(attitude.attitude_id) {
            return Err(ShenkoError::conflict(format!(
                "attitude {} appears twice in one per-step projection; refusing to guess which row is current",
                attitude.attitude_id,
            )));
        }
        let Some(key) = attitude_application_key(&attitude.role, &attitude.digest) else {
            return Err(ShenkoError::conflict(format!(
                "attitude {} has role `{}`, which is not part of the per-step application set; the ledger must be partitioned before a projection is planned (docs/specs/cognition-units.md#spec-51-1)",
                attitude.attitude_id,
                attitude.role.kind(),
            )));
        };
        if !keys.insert(key) {
            return Err(ShenkoError::conflict(format!(
                "two current attitudes claim the same application key ({} / {}); refusing to present both as in force",
                attitude.role.kind(),
                attitude.digest,
            )));
        }
    }
    let mut units = BTreeSet::new();
    for metapackage in metapackages {
        if metapackage.unit_id.trim().is_empty() || metapackage.revision.trim().is_empty() {
            return Err(ShenkoError::conflict(
                "a metapackage fact requires a unit identity and a revision",
            ));
        }
        if !units.insert(metapackage.unit_id.as_str()) {
            return Err(ShenkoError::conflict(format!(
                "metapackage {} is present twice; an installation has one current version of one unit",
                metapackage.unit_id,
            )));
        }
    }
    Ok(())
}

fn build_page(
    step_no: u64,
    ledger: Option<&AttitudeLedgerFacts>,
    attitudes: &[AppliedAttitude],
    metapackages: &[AppliedMetapackage],
    shown_attitudes: usize,
    shown_metapackages: usize,
) -> Result<AttitudeApplication, ShenkoError> {
    let mut text = String::from(HEADER);
    if !attitudes.is_empty() {
        text.push_str(&attitude_section(step_no, ledger, attitudes, shown_attitudes)?);
    }
    if !metapackages.is_empty() {
        text.push_str(&metapackage_section(step_no, metapackages, shown_metapackages)?);
    }
    let receipt = receipt_of(
        step_no,
        ledger,
        attitudes,
        metapackages,
        shown_attitudes,
        shown_metapackages,
    );
    text.push('\n');
    text.push_str(RECEIPT_PREFIX);
    text.push_str(&serde_json::to_string(&receipt).map_err(|error| {
        ShenkoError::conflict(format!("attitude application receipt serializes: {error}"))
    })?);
    text.push('\n');
    Ok(AttitudeApplication { text, receipt })
}

fn receipt_of(
    step_no: u64,
    ledger: Option<&AttitudeLedgerFacts>,
    attitudes: &[AppliedAttitude],
    metapackages: &[AppliedMetapackage],
    shown_attitudes: usize,
    shown_metapackages: usize,
) -> AttitudeApplicationReceipt {
    let mut revisions = RevisionReceipt {
        projected: 0,
        unresolved: 0,
    };
    for attitude in rotated(attitudes, start_index(step_no, attitudes.len())).take(shown_attitudes) {
        match &attitude.revision {
            RevisionView::Projected { .. } => revisions.projected += 1,
            RevisionView::Unresolved { .. } => revisions.unresolved += 1,
            RevisionView::NotApplicable => {}
        }
    }
    AttitudeApplicationReceipt {
        schema: ATTITUDE_APPLICATION_RECEIPT_SCHEMA,
        rotation: step_no,
        ledger: ledger.cloned(),
        attitudes: SectionReceipt {
            total: attitudes.len(),
            shown: shown_attitudes,
            omitted: attitudes.len().saturating_sub(shown_attitudes),
            start: start_index(step_no, attitudes.len()),
        },
        metapackages: if metapackages.is_empty() {
            None
        } else {
            Some(SectionReceipt {
                total: metapackages.len(),
                shown: shown_metapackages,
                omitted: metapackages.len().saturating_sub(shown_metapackages),
                start: start_index(step_no, metapackages.len()),
            })
        },
        revisions,
        roles_not_applied: ROLES_NOT_APPLIED.iter().map(|role| (*role).to_string()).collect(),
        bound_bytes: MAX_ATTITUDE_APPLICATION_BYTES,
    }
}

fn attitude_section(
    step_no: u64,
    ledger: Option<&AttitudeLedgerFacts>,
    attitudes: &[AppliedAttitude],
    shown: usize,
) -> Result<String, ShenkoError> {
    let total = attitudes.len();
    let start = start_index(step_no, total);
    let mut text = String::from(ATTITUDE_HEADER);
    for (offset, attitude) in rotated(attitudes, start).take(shown).enumerate() {
        text.push_str(&attitude_line(start + offset, attitude)?);
        text.push('\n');
    }
    text.push_str(&attitude_footer(step_no, ledger, total, start, shown));
    Ok(text)
}

fn attitude_footer(
    step_no: u64,
    ledger: Option<&AttitudeLedgerFacts>,
    total: usize,
    start: usize,
    shown: usize,
) -> String {
    let mut footer = if shown >= total {
        format!("（共 {total} 条，已全部展示；轮转 {step_no}，起点 {start}")
    } else {
        format!(
            "（共 {total} 条，本步展示 {shown} 条；轮转 {step_no}，起点 {start}；预算之外 {} 条仍在账本上，字节不变，没有删除也没有停用，下一步从索引 {} 继续",
            total - shown,
            // The page advances one canonical position per durable step, not
            // by however many entries happened to fit.
            (start + 1) % total,
        )
    };
    if let Some(ledger) = ledger.filter(|ledger| ledger.read_truncated) {
        footer.push_str(&format!(
            "；本次读取窗口 {} 行已用满，更早的当前态度可能未读入（不表示不存在）",
            ledger.read_limit,
        ));
    }
    footer.push_str("）\n");
    footer
}

fn metapackage_section(step_no: u64, metapackages: &[AppliedMetapackage], shown: usize) -> Result<String, ShenkoError> {
    let total = metapackages.len();
    let start = start_index(step_no, total);
    let mut text = String::from(METAPACKAGE_HEADER);
    for (offset, metapackage) in rotated(metapackages, start).take(shown).enumerate() {
        text.push_str(&metapackage_line(start + offset, metapackage)?);
        text.push('\n');
    }
    if shown >= total {
        text.push_str(&format!(
            "（共 {total} 个已安装的宏包版本，已全部展示；轮转 {step_no}，起点 {start}）\n"
        ));
    } else {
        text.push_str(&format!(
            "（共 {total} 个已安装的宏包版本，本步展示 {shown} 个；轮转 {step_no}，起点 {start}；预算之外 {} 个仍在安装中，本步未展示，下一步从索引 {} 继续）\n",
            total - shown,
            (start + 1) % total,
        ));
    }
    text
}

fn attitude_line(index: usize, attitude: &AppliedAttitude) -> Result<String, ShenkoError> {
    let mut value = json!({
        "index": index,
        "attitude_id": attitude.attitude_id,
        "content_digest": attitude.digest,
        "role": attitude.role.kind(),
        "reading": attitude.reading,
        "confidence": attitude.confidence,
    });
    match &attitude.role {
        AttitudeRole::Adoption(target) => {
            value["unit_id"] = json!(target.unit_id);
            if let Some(version) = target.unit_version.as_deref() {
                value["unit_version"] = json!(version);
            }
            if let Some(member) = target.member.as_deref() {
                value["member"] = json!(member);
            }
        }
        AttitudeRole::Definition(slot) => value["slot"] = json!(slot),
        AttitudeRole::Revision { supersedes } => value["supersedes"] = json!(supersedes),
        AttitudeRole::Projection(stage) => value["stage"] = json!(stage),
        // The planner refuses these roles; they never reach a line.
        AttitudeRole::Assertion | AttitudeRole::Unfinished | AttitudeRole::EffectUnknown => {}
    }
    match &attitude.revision {
        RevisionView::NotApplicable => {}
        RevisionView::Projected { selection, taste } => {
            let mut revisions = selection.project_json();
            if let Some(object) = revisions.as_object_mut() {
                object.insert(
                    "taste".into(),
                    json!({
                        "unit_id": taste.id,
                        "revision": taste.revision,
                        "content_digest": taste.content_digest,
                    }),
                );
            }
            value["revisions"] = revisions;
        }
        RevisionView::Unresolved { detail } => {
            value["revisions"] = json!({"unresolved": true, "detail": detail});
        }
    }
    serde_json::to_string(&value).map_err(|error| {
        ShenkoError::conflict(format!("attitude application line serializes: {error}"))
    })
}

fn metapackage_line(index: usize, metapackage: &AppliedMetapackage) -> Result<String, ShenkoError> {
    let dependencies: Vec<Value> = metapackage
        .dependencies
        .iter()
        .map(|dependency| {
            let selected = match (&dependency.selected_unit_id, &dependency.selected_revision) {
                (Some(unit_id), selected_revision) => json!({
                    "unit_id": unit_id,
                    "revision": selected_revision,
                    "artifact_digest": dependency.selected_artifact_digest,
                }),
                (None, _) => Value::Null,
            };
            json!({
                "edge": dependency.edge,
                "strength": dependency.strength,
                "status": dependency.status,
                "selected": selected,
                "reason": dependency.reason,
                "reason_truncated": dependency.reason_truncated,
            })
        })
        .collect();
    let mut value = json!({
        "index": index,
        "unit_id": metapackage.unit_id,
        "revision": metapackage.revision,
        "artifact_digest": metapackage.artifact_digest,
        "state": metapackage.state,
        "enabled": metapackage.enabled,
        "capability_card": {
            "preview": metapackage.card_preview,
            "truncated": metapackage.card_truncated,
        },
        "adoption": metapackage.adoption,
        "dependencies": dependencies,
    });
    if metapackage.dependencies_omitted > 0 {
        value["dependencies_omitted"] = json!(metapackage.dependencies_omitted);
    }
    serde_json::to_string(&value).map_err(|error| {
        ShenkoError::conflict(format!("metapackage application line serializes: {error}"))
    })
}

fn rotated<T>(values: &[T], start: usize) -> impl Iterator<Item = &T> {
    values.iter().cycle().skip(start).take(values.len())
}

fn start_index(step_no: u64, total: usize) -> usize {
    if total == 0 {
        0
    } else {
        (step_no % total as u64) as usize
    }
}

/// Byte-bounded preview of free text. The cut respects UTF-8 scalar boundaries
/// and marks itself, so a preview never looks like the whole text.
fn bounded(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut preview = text[..end].to_string();
    preview.push('…');
    preview
}

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_context_policy::taste::{
        DefaultRevisionTaste, DisclosureRequest, RevisionChain, RevisionMeta, RevisionTaste,
    };

    fn digest(seed: &str) -> Digest {
        Digest::blake3(seed.as_bytes())
    }

    fn attitude(index: usize, role: AttitudeRole) -> AppliedAttitude {
        let digest = digest(&format!("content-{index}"));
        AppliedAttitude {
            attitude_id: EntityId::new(),
            digest,
            role,
            reading: ReasonReading::Fact,
            confidence: ConfidenceState::Unknown,
            revision: RevisionView::NotApplicable,
        }
    }

    fn metapackage(index: usize) -> AppliedMetapackage {
        AppliedMetapackage {
            unit_id: format!("metapackage-{index:02}"),
            revision: "1.0.0".to_string(),
            artifact_digest: digest(&format!("artifact-{index}")).to_string(),
            state: "enabled".to_string(),
            enabled: true,
            card_preview: "a group of units".to_string(),
            card_truncated: false,
            adoption: Some(AppliedAdoption {
                decision: "adopted".to_string(),
                method: "sandbox_comparison".to_string(),
                confidence: Some(0.75),
                evidence_verified: true,
                empirical: true,
                measurement: Some("measured".to_string()),
            }),
            dependencies: Vec::new(),
            dependencies_omitted: 0,
        }
    }

    fn entry_lines(text: &str) -> Vec<Value> {
        text.lines()
            .filter(|line| line.starts_with('{'))
            .map(|line| serde_json::from_str(line).expect("every emitted entry line is whole JSON"))
            .collect()
    }

    /// Nothing applies: no paragraph is manufactured, and the receipt still
    /// states the rotation and the empty page.
    #[test]
    fn empty_material_produces_no_block_and_an_honest_receipt() {
        let ledger = AttitudeLedgerFacts {
            ledger_id: EntityId::new().to_string(),
            upper_commit_seq: 0,
            read_limit: ATTITUDE_APPLICATION_READ_LIMIT,
            read_truncated: false,
        };
        let page = plan_attitude_application(7, Some(ledger.clone()), &[], &[]).unwrap();
        assert!(page.text.is_empty(), "absence of material is not a prompt paragraph");
        assert_eq!(page.receipt.rotation, 7);
        assert_eq!(page.receipt.ledger.as_ref(), Some(&ledger));
        assert_eq!(page.receipt.attitudes.total, 0);
        assert_eq!(page.receipt.attitudes.shown, 0);
        assert!(page.receipt.metapackages.is_none());
        assert_eq!(page.receipt.revisions.projected, 0);
        assert_eq!(
            page.receipt.roles_not_applied,
            ROLES_NOT_APPLIED.iter().map(|role| role.to_string()).collect::<Vec<_>>()
        );
    }

    /// Over the bound: whole entries only, an explicit omission count, the
    /// continuation index, and a receipt whose JSON line round-trips to the
    /// returned receipt.
    #[test]
    fn oversized_material_is_cut_to_whole_entries_with_an_explicit_omission() {
        let attitudes: Vec<AppliedAttitude> = (0..80)
            .map(|index| attitude(index, AttitudeRole::Definition(format!("slot-{index:02}"))))
            .collect();
        let metapackages: Vec<AppliedMetapackage> = (0..40).map(metapackage).collect();
        let ledger = AttitudeLedgerFacts {
            ledger_id: EntityId::new().to_string(),
            upper_commit_seq: 4242,
            read_limit: ATTITUDE_APPLICATION_READ_LIMIT,
            read_truncated: true,
        };
        let page = plan_attitude_application(11, Some(ledger), &attitudes, &metapackages).unwrap();
        assert!(page.text.len() <= MAX_ATTITUDE_APPLICATION_BYTES,
            "the whole block stayed inside the bound: {} bytes", page.text.len());
        assert!(page.receipt.attitudes.shown > 0, "some attitudes are shown");
        assert!(page.receipt.attitudes.shown < attitudes.len(),
            "the oversized page is a window, not everything");
        assert_eq!(page.receipt.attitudes.omitted,
            attitudes.len() - page.receipt.attitudes.shown);
        let metapackages_receipt = page.receipt.metapackages.as_ref().expect("metapackages receipt");
        assert_eq!(metapackages_receipt.omitted,
            metapackages.len() - metapackages_receipt.shown);
        // The omission is stated to the reader, with the index the next
        // durable step continues from (one canonical position, not `shown`).
        assert!(page.text.contains(&format!(
            "预算之外 {} 条仍在账本上，字节不变，没有删除也没有停用，下一步从索引 {} 继续",
            page.receipt.attitudes.omitted,
            (page.receipt.attitudes.start + 1) % attitudes.len(),
        )));
        // Nothing was cut mid-identity: every entry line parses and its
        // identities are exactly the receipt's identities.
        let entries = entry_lines(&page.text);
        let shown = entries.iter().filter(|entry| entry.get("attitude_id").is_some()).count();
        assert_eq!(shown, page.receipt.attitudes.shown);
        // The receipt line in the prompt is the returned receipt, byte for byte.
        let receipt_line = page.text.lines()
            .find(|line| line.starts_with(RECEIPT_PREFIX)).expect("receipt line");
        let parsed: AttitudeApplicationReceipt = serde_json::from_str(
            receipt_line.trim_start_matches(RECEIPT_PREFIX)).unwrap();
        assert_eq!(parsed, page.receipt);
        assert!(parsed.ledger.expect("ledger").read_truncated);
    }

    /// Two current attitudes under one application key, or one metapackage
    /// twice, are refused by name: the projection never picks a side.
    #[test]
    fn conflicting_input_is_refused_instead_of_picked() {
        let first = attitude(1, AttitudeRole::Definition("fact.meaning".to_string()));
        let second = attitude(2, AttitudeRole::Definition("fact.meaning".to_string()));
        let error = plan_attitude_application(0, None, &[first.clone(), second.clone()], &[])
            .unwrap_err();
        assert!(error.to_string().contains("same application key"), "{error}");

        let duplicate = vec![first.clone(), first];
        let error = plan_attitude_application(0, None, &duplicate, &[]).unwrap_err();
        assert!(error.to_string().contains("appears twice"), "{error}");

        let same_metapackage = vec![metapackage(1), metapackage(1)];
        let error = plan_attitude_application(0, None, &[], &same_metapackage).unwrap_err();
        assert!(error.to_string().contains("present twice"), "{error}");

        // A role outside the application set is a caller error, not a row to hide.
        let assertion = attitude(3, AttitudeRole::Assertion);
        let error = plan_attitude_application(0, None, &[assertion], &[]).unwrap_err();
        assert!(error.to_string().contains("not part of the per-step application set"), "{error}");
    }

    /// A revision view is either the installed taste's projection or an
    /// explicit unresolved marker — never an invented count.
    #[test]
    fn revision_views_are_projected_or_reported_unresolved() {
        let taste = DefaultRevisionTaste.unit();
        let mut revised = attitude(9, AttitudeRole::Definition("fact.meaning".to_string()));
        revised.revision = RevisionView::Projected {
            selection: DefaultRevisionTaste
                .disclose(
                    &RevisionChain::new(
                        "attitude.definition",
                        EntityId::new(),
                        vec![RevisionMeta {
                            revision: 3,
                            content_digest: Some(digest("content-9").to_string()),
                            written_at_ms: 90,
                        }],
                    )
                    .unwrap()
                    .with_canonical(CanonicalDisclosure::new(digest("content-9").to_string(), 2, 90)),
                    DisclosureRequest::Selected,
                )
                .unwrap(),
            taste,
        };
        let mut unresolved = attitude(4, AttitudeRole::Revision { supersedes: digest("older") });
        unresolved.revision = RevisionView::Unresolved {
            detail: "TASTE_UNAVAILABLE: no revision-projection taste is installed on this node; the older-version count and latest modification time are not projected".to_string(),
        };
        let page = plan_attitude_application(0, None, &[revised, unresolved], &[]).unwrap();
        assert_eq!(page.receipt.revisions.projected, 1);
        assert_eq!(page.receipt.revisions.unresolved, 1);
        let entries = entry_lines(&page.text);
        let projected = entries.iter().find(|entry| entry.get("slot").is_some()).unwrap();
        assert_eq!(projected["revisions"]["older_versions"], json!(2));
        assert_eq!(projected["revisions"]["full_text"], Value::Null);
        assert_eq!(projected["revisions"]["taste"]["unit_id"], json!(DefaultRevisionTaste::UNIT));
        let unresolved = entries.iter().find(|entry| entry.get("supersedes").is_some()).unwrap();
        assert_eq!(unresolved["revisions"]["unresolved"], json!(true));
        assert!(unresolved["revisions"]["older_versions"].is_null());
    }

    /// The rotation is the whole selection rule: consecutive steps page
    /// through the canonical order, so no entry is starved by a lexical gate.
    #[test]
    fn rotation_pages_through_every_entry_without_a_keyword_gate() {
        let attitudes: Vec<AppliedAttitude> = (0..5)
            .map(|index| attitude(index, AttitudeRole::Definition(format!("slot-{index}"))))
            .collect();
        let mut seen: Vec<String> = Vec::new();
        for step in 0..5 {
            let page = plan_attitude_application(step, None, &attitudes, &[]).unwrap();
            assert_eq!(page.receipt.attitudes.shown, 5, "small pages show everything");
            let start = page.receipt.attitudes.start;
            assert_eq!(start, step as usize);
            for entry in entry_lines(&page.text) {
                if let Some(slot) = entry.get("slot").and_then(Value::as_str) {
                    seen.push(slot.to_string());
                }
            }
            // Retrying one step is the same page.
            let retry = plan_attitude_application(step, None, &attitudes, &[]).unwrap();
            assert_eq!(retry.text, page.text);
        }
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 5, "every attitude is reachable by rotation");
    }
}
