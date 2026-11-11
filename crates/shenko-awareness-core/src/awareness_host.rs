//! The installable awareness capability host — docs/specs/cognition-units.md#spec-51-2, docs/specs/cognition-units.md#spec-51-3, docs/specs/cognition-units.md#spec-51-8.
//!
//! awareness is a capability, not a document the model can be assumed to
//! follow. This module owns the installable half: a host that is resident only
//! while the distribution actually supplies the capability, and that offers
//! the subject a bounded, attributed notice instead of a command.
//!
//! What a notice is: one installed atomic attention material, the concrete
//! fact this node observed when the material is about observation, and where
//! the reading stops. What it is not: authority, a dispatch, a stored
//! attitude, or a permission. The text is harness — when this capability
//! enters the model's own cognition the harness can get thinner, and nothing
//! here pretends otherwise.
//!
//! Residency is not decided by a unit id. The distribution says which installed
//! unit provides `cap.awareness.host/1` (the `capability_providers` preinstall
//! row: contract id, major, provider unit id); the host verifies that the
//! selected unit is installed, current, enabled and really declares the
//! contract, and otherwise reports itself absent. A metapackage depends on the
//! contract, never on the provider's unit id, so replacing the provider keeps
//! the capability.
//!
//! Attention atoms are metapackage members, so one joins the offered material
//! only after an individual authenticated adoption attitude exists for exactly
//! that content: the Node's own recorded adoption, adopted and verified. Merely
//! being installed, generated or generated-and-enabled is not adoption.
//!
//! Seam to the content/attitude store: a notice is harness, not a stored
//! attitude. It carries the material, the observation it rests on and the
//! perspective it was read through, so the subject can later write the notice
//! down as immutable content plus one attitude whose reason terminates at that
//! observation (`Observation`) or at the installed content (`Source`), with the
//! modeled-other attribution travelling as a perspective reason. This module
//! deliberately declares no confidence, no attitude id and no second store.

use std::borrow::Cow;
use std::convert::Infallible;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use cordis_core::{Context, Plugin, Service};
use serde::{Deserialize, Serialize};
use shenko_node::{InstalledUnitFact, UnitKind};
use shenko_types::object::Digest;

use crate::cognition::{
    CognitionCatalog, CognitionPackage, CognitionState, CognitionUnit, MAX_CARD_BYTES,
    parse_cognition_entry, truncate_utf8,
};
use crate::perception::{PerceptionPerspective, PerceptionStatus};

/// The capability contract the host provides, without its version.
pub const HOST_CONTRACT: &str = "cap.awareness.host";
/// The capability contract one atomic attention material provides.
pub const ATTENTION_CONTRACT: &str = "cap.awareness.attention";
/// The major version this host implements.
pub const HOST_CONTRACT_MAJOR: u32 = 1;
/// How many notices one projection may carry.
pub const MAX_NOTICES: usize = 8;
/// The bound of one rendered notice line.
pub const MAX_NOTICE_BYTES: usize = 640;
/// The bound of one rendered notice block.
pub const MAX_NOTICE_BLOCK_BYTES: usize = 4 * 1024;

/// One versioned capability contract, e.g. `cap.awareness.host/1`.
///
/// The grammar is `<contract_id>/<major>`; a declared token without a numeric
/// major is an abstract topic (R138), not a capability contract, so it never
/// satisfies a contract edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityContract {
    pub contract_id: String,
    pub major: u32,
}

impl CapabilityContract {
    /// Parse one declared capability token.
    pub fn parse(token: &str) -> Option<Self> {
        let token = token.trim();
        let (contract_id, major) = token.rsplit_once('/')?;
        let major: u32 = major.trim().parse().ok()?;
        (!contract_id.trim().is_empty() && major > 0).then(|| Self {
            contract_id: contract_id.trim().to_string(),
            major,
        })
    }

    /// The declared token for this contract.
    pub fn token(&self) -> String {
        format!("{}/{major}", self.contract_id, major = self.major)
    }

    /// Whether a unit's `provides` declaration carries exactly this contract.
    pub fn declared_by(&self, provides: &[String]) -> bool {
        provides
            .iter()
            .filter_map(|token| Self::parse(token))
            .any(|declared| declared == *self)
    }
}

/// One row of the distribution's capability-provider table.
///
/// This is data, not a hardcoded unit id: the distribution states which
/// installed unit currently provides the contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitySelection {
    pub contract_id: String,
    pub major: u32,
    pub unit_id: String,
}

impl CapabilitySelection {
    pub fn host(unit_id: impl Into<String>) -> Self {
        Self {
            contract_id: HOST_CONTRACT.to_string(),
            major: HOST_CONTRACT_MAJOR,
            unit_id: unit_id.into(),
        }
    }

    /// The exact contract this row names.
    pub fn contract(&self) -> CapabilityContract {
        CapabilityContract {
            contract_id: self.contract_id.clone(),
            major: self.major,
        }
    }
}

/// One installed unit version, as the host names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitSelection {
    pub unit_id: String,
    pub revision: String,
    /// Digest of the content this identity is bound to, when the authority
    /// reports one.
    pub content_digest: String,
    pub kind: String,
}

/// Whether the capability host is actually resident, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostState {
    /// True only when the selected provider is installed, current, enabled and
    /// declares the contract. The service being registered is mechanism; this
    /// flag is the capability.
    pub resident: bool,
    pub selection: CapabilitySelection,
    /// The installed provider this row selected, when it is installed at all.
    pub provider: Option<UnitSelection>,
    /// Whether the selected unit's own declaration carries the contract.
    pub declared: bool,
    /// Why the host is resident or absent — a fact about this installation.
    pub reason: String,
}

/// Where a notice's reading stops — docs/specs/cognition-units.md#spec-51-1.
///
/// A reading that stops at an observation of this installation reads like a
/// fact; a reading that stops at installed authored content reads like a
/// conclusion. Neither is a stored attitude here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NoticeReason {
    /// The installed content this attention comes from.
    AuthoredContent {
        unit_id: String,
        revision: String,
        content_digest: String,
    },
    /// An observation this installation registered for the exact revision.
    Observation {
        unit_id: String,
        revision: String,
        content_digest: String,
        /// Digest of the archived observation bytes the reading rests on.
        observation_digest: String,
        observed_at: String,
    },
}

/// Whose perception a notice's observation was read through.
///
/// A modeled other subject is perspective taking, never this subject's own
/// fact. An undeclared attribution stays undeclared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NoticeAttribution {
    Own,
    ModeledOther { subject: String },
    Undeclared,
}

/// The attribution a bound perspective gives a reading.
pub fn attribution_of(perspective: Option<&PerceptionPerspective>) -> NoticeAttribution {
    match perspective {
        Some(PerceptionPerspective::Own) => NoticeAttribution::Own,
        Some(PerceptionPerspective::ModeledOther { subject }) => NoticeAttribution::ModeledOther {
            subject: subject.clone(),
        },
        None => NoticeAttribution::Undeclared,
    }
}

/// One bounded advisory notice.
///
/// There is deliberately no confidence field and no attitude here: an attitude
/// is a ledger record pointing at one content revision with a reason pointer,
/// and that store has one owner. Inventing a second confidence scale in the
/// harness would be a second source of truth, so a notice only carries what it
/// can observe and where its reading stops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwarenessNotice {
    pub atom: UnitSelection,
    /// The one attention this atom carries, bounded.
    pub attention: String,
    /// Why this attention is worth taking, as the material authored it.
    pub why: Option<String>,
    /// When it tends to apply.
    pub applicability: Option<String>,
    /// When it usually does not apply.
    pub negative_cues: Vec<String>,
    /// Where the reading stops.
    pub reason: NoticeReason,
    /// Whose perception the observation was read through, when there is one.
    pub attribution: NoticeAttribution,
    /// The fixed statement that the notice grants nothing. It is a constant so
    /// no producer can dress a notice up as authority.
    pub authority: &'static str,
}

/// The one value every notice carries as its authority: advisory only.
pub const NOTICE_AUTHORITY: &str =
    "advisory harness: this notice grants no permission, dispatches no action, and is not a command";

impl AwarenessNotice {
    /// The modeled other subject this reading was taken through, when there is
    /// one. A caller writing the notice down puts this on a perspective reason;
    /// it never becomes the subject's own observation.
    pub fn perspective_subject(&self) -> Option<&str> {
        match &self.attribution {
            NoticeAttribution::ModeledOther { subject } => Some(subject),
            NoticeAttribution::Own | NoticeAttribution::Undeclared => None,
        }
    }

    /// One bounded line for the prompt.
    pub fn render(&self) -> String {
        let mut line = format!(
            "- [{}@{}] {}",
            self.atom.unit_id, self.atom.revision, self.attention
        );
        if let Some(why) = self.why.as_deref() {
            line.push_str(&format!("（为何值得注意：{why}）"));
        }
        if let Some(applicability) = self.applicability.as_deref() {
            line.push_str(&format!("（适用条件：{applicability}）"));
        }
        if !self.negative_cues.is_empty() {
            line.push_str(&format!("（通常不适用：{}）", self.negative_cues.join("；")));
        }
        match &self.reason {
            NoticeReason::AuthoredContent { .. } => {
                line.push_str("（理由停在已安装的这段内容上）");
            }
            NoticeReason::Observation {
                observation_digest,
                observed_at,
                ..
            } => {
                line.push_str(&format!(
                    "（理由停在本机 {observed_at} 那次观察上，观察摘要 {observation_digest}）"
                ));
            }
        }
        match &self.attribution {
            NoticeAttribution::Own => line.push_str("（视角：本主体）"),
            NoticeAttribution::ModeledOther { subject } => line.push_str(&format!(
                "（视角：模拟 {subject}；是推测对方会这样看，不是本主体的观察）"
            )),
            NoticeAttribution::Undeclared => {
                line.push_str("（视角未声明，不能当作本主体自己的观察）")
            }
        }
        truncate_utf8(&line, MAX_NOTICE_BYTES).0
    }
}

/// The installable awareness capability host.
///
/// The host holds no state of its own: residency and offered material are
/// projected from the installation authority on every call, so an update,
/// disable or uninstall is visible without a restart and no stale capability
/// survives in memory.
pub struct AwarenessHost {
    catalog: Arc<CognitionCatalog>,
    selection: CapabilitySelection,
    adoption: Arc<dyn AtomAdoption>,
}

impl AwarenessHost {
    /// The host for one distribution selection.
    pub fn new(catalog: Arc<CognitionCatalog>, selection: CapabilitySelection) -> Self {
        Self {
            catalog,
            selection,
            adoption: Arc::new(InstallationAdoption),
        }
    }

    /// Ask another authenticated adoption authority whether one install
    /// version was adopted here.
    ///
    /// The default asks the installation authority (the Node's own verified
    /// adoption record). The content/attitude store publishes its own adoption
    /// query; wiring it in replaces the gate instead of adding a second one.
    pub fn with_adoption(mut self, adoption: Arc<dyn AtomAdoption>) -> Self {
        self.adoption = adoption;
        self
    }

    /// The distribution row this host was built from.
    pub fn selection(&self) -> &CapabilitySelection {
        &self.selection
    }

    /// Whether the capability is resident on this installation right now.
    pub fn state(&self) -> Result<HostState> {
        let contract = self.selection.contract();
        let units = self.catalog.units()?;
        let provider = units
            .iter()
            .find(|unit| unit.unit_id == self.selection.unit_id);
        let Some(provider) = provider else {
            return Ok(HostState {
                resident: false,
                selection: self.selection.clone(),
                provider: None,
                declared: false,
                reason: format!(
                    "安装权威里没有提供者单元 '{}'（契约 {}）；能力未安装，不是选项",
                    self.selection.unit_id,
                    contract.token()
                ),
            });
        };
        let selected = selection_of(provider);
        let declared = contract.declared_by(&provider.provides);
        let enabled = provider.enabled;
        let current = provider.current;
        // Supplying a capability is itself an adoption decision: a provider
        // that was never adopted here, or whose adoption evidence this
        // installation could not verify, leaves the slot unselected instead of
        // being resident by mere installation.
        let adopted = self.adoption.adopted(provider);
        let reason = if !declared {
            format!(
                "单元 '{}@{}' 没有声明契约 {}；同名不代表有能力",
                provider.unit_id,
                provider.revision,
                contract.token()
            )
        } else if !current {
            format!(
                "单元 '{}@{}' 不是当前版本；提供者已被替换",
                provider.unit_id, provider.revision
            )
        } else if !enabled {
            format!(
                "单元 '{}@{}' 已停用；能力随提供者一起离开",
                provider.unit_id, provider.revision
            )
        } else if !adopted {
            format!(
                "单元 '{}@{}' 已安装但没有本机核实过的采纳态度；装进来不等于这个能力在此被采纳",
                provider.unit_id, provider.revision
            )
        } else {
            format!(
                "单元 '{}@{}' 已安装、当前、启用、已采纳，并声明 {}",
                provider.unit_id,
                provider.revision,
                contract.token()
            )
        };
        Ok(HostState {
            resident: declared && enabled && current && adopted,
            selection: self.selection.clone(),
            provider: Some(selected),
            declared,
            reason,
        })
    }

    /// The bounded notice material this installation offers right now.
    ///
    /// Empty when the capability is not resident: an absent capability does
    /// not fall back to documents that merely exist on disk.
    pub fn notices(&self, limit: usize) -> Result<Vec<AwarenessNotice>> {
        if !self.state()?.resident {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, MAX_NOTICES);
        let units = self.catalog.units()?;
        // One card projection reconciles the observation sources for the exact
        // installed revisions, so a notice can only rest on an observation the
        // catalog itself admits for this version.
        let cards = self.catalog.cards()?;
        let mut notices = Vec::new();
        for unit in &units {
            if notices.len() >= limit {
                break;
            }
            if !is_offered(unit) || !self.adoption.adopted(unit) {
                continue;
            }
            if !attention_material(unit) {
                continue;
            }
            let atom = selection_of(unit);
            let attention = truncate_utf8(unit.capability_card.trim(), MAX_CARD_BYTES).0;
            if attention.is_empty() {
                continue;
            }
            let mut notice = AwarenessNotice {
                atom: atom.clone(),
                attention,
                why: unit.why.clone(),
                applicability: unit.applicability.clone(),
                negative_cues: unit.negative_cues.clone(),
                reason: NoticeReason::AuthoredContent {
                    unit_id: atom.unit_id.clone(),
                    revision: atom.revision.clone(),
                    content_digest: atom.content_digest.clone(),
                },
                attribution: NoticeAttribution::Own,
                authority: NOTICE_AUTHORITY,
            };
            if unit.kind == UnitKind::Perception {
                // A perception atom's notice rests on the observation this
                // node holds for the exact revision, when it holds one. The
                // card carries the observation of this exact card revision, so
                // an observation of a replaced version can never answer here.
                let Some(card) = cards
                    .iter()
                    .find(|card| card.unit_id == unit.unit_id && card.revision == unit.revision)
                else {
                    continue;
                };
                let Some(PerceptionStatus::Observed { fact }) = card.perception.as_ref() else {
                    // An unbound, unobserved or unavailable perception is not
                    // material this host can hand over as an observation.
                    continue;
                };
                let Some(observation_digest) = fact.evidence_digest.clone() else {
                    continue;
                };
                notice.reason = NoticeReason::Observation {
                    unit_id: atom.unit_id.clone(),
                    revision: atom.revision.clone(),
                    content_digest: atom.content_digest.clone(),
                    observation_digest,
                    observed_at: fact.observed_at.clone(),
                };
                let binding = crate::perception::PerceptionBinding {
                    unit_id: unit.unit_id.clone(),
                    revision: unit.revision.clone(),
                    artifact_digest: unit.artifact_digest.clone(),
                };
                notice.attribution =
                    attribution_of(self.catalog.perception().perspective(&binding).as_ref());
            }
            notices.push(notice);
        }
        Ok(notices)
    }

    /// The bounded notice block for one projection, or `None` when this
    /// installation does not currently have the capability.
    ///
    /// The block states its own harness status and authority, so nothing in
    /// the prompt can read a notice as an instruction the runtime enforces.
    pub fn render_notices(&self, limit: usize) -> Result<Option<String>> {
        let state = self.state()?;
        if !state.resident {
            return Ok(None);
        }
        let notices = self.notices(limit)?;
        if notices.is_empty() {
            return Ok(None);
        }
        let mut text = String::from(
            "已安装的 awareness 能力给出的有界 notice（供你注意，不是命令；是否采纳由你判断）：\n",
        );
        for notice in &notices {
            let line = notice.render();
            if text.len() + line.len() + 1 > MAX_NOTICE_BLOCK_BYTES {
                break;
            }
            text.push_str(&line);
            text.push('\n');
        }
        text.push_str(&format!("（{NOTICE_AUTHORITY}）\n"));
        Ok(Some(truncate_utf8(&text, MAX_NOTICE_BLOCK_BYTES).0))
    }
}

impl Service for AwarenessHost {
    const NAME: &'static str = "shenko.awareness.host/v1";
}

/// Publishes the host so the subject's machinery can find the one projection
/// by name. Registering the service is mechanism; `HostState::resident` is the
/// capability, and every consumer must read it.
pub struct AwarenessHostPlugin;

impl Plugin for AwarenessHostPlugin {
    type Config = Arc<AwarenessHost>;
    type Input = Arc<AwarenessHost>;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("awareness-host")
    }

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        let _publication = ctx
            .provide(Arc::clone(input))
            .map_err(std::io::Error::other)?;
        Ok(())
    }
}

fn selection_of(unit: &CognitionUnit) -> UnitSelection {
    UnitSelection {
        unit_id: unit.unit_id.clone(),
        revision: unit.revision.clone(),
        content_digest: unit.artifact_digest.clone(),
        kind: unit.kind.as_str().to_string(),
    }
}

/// Whether this unit version is offered at all: current, enabled and in an
/// offered state.
fn is_offered(unit: &CognitionUnit) -> bool {
    unit.current
        && unit.enabled
        && matches!(
            unit.state,
            CognitionState::Enabled | CognitionState::Candidate | CognitionState::Adopted
        )
}

/// Whether an individual authenticated adoption attitude exists for exactly
/// this content.
///
/// The Node verifies every piece of adoption evidence against its own durable
/// state before recording the decision, and the decision is bound to the exact
/// artifact identity. So "adopted and evidence verified" is this
/// installation's own authenticated attitude for this revision — not an
/// imported claim, and not a generation side effect.
fn adopted_here(unit: &CognitionUnit) -> bool {
    unit.adoption.as_ref().is_some_and(|adoption| {
        if adoption.decision != "adopted" || !adoption.evidence_verified {
            return false;
        }
        let verified: Vec<&crate::cognition::AdoptionEvidenceFact> =
            adoption.evidence.iter().filter(|fact| fact.verified).collect();
        // The attitude must cite at least one verified fact of this
        // installation, and no verified citation may name another content
        // digest than this member's own.
        !verified.is_empty()
            && verified.iter().all(|fact| {
                fact.digest
                    .as_deref()
                    .is_none_or(|digest| digest == unit.artifact_digest)
            })
    })
}

/// Whether this installed unit version is attention material at all.
///
/// An awareness unit is material by kind. A Skill is material only when it
/// declares the attention contract — a skill is a way of doing something, not
/// an attention, until it says it provides one. A perception unit teaches how
/// to observe, so it may contribute an observation-backed notice. Code is never
/// attention material.
fn attention_material(unit: &CognitionUnit) -> bool {
    let attention = CapabilityContract {
        contract_id: ATTENTION_CONTRACT.to_string(),
        major: HOST_CONTRACT_MAJOR,
    };
    match unit.kind {
        UnitKind::Awareness | UnitKind::Perception => true,
        UnitKind::Skill => attention.declared_by(&unit.provides),
        UnitKind::Code | UnitKind::Metapackage | UnitKind::Distribution => false,
    }
}

/// The attention units a metapackage member must carry: awareness material by
/// kind, or any unit that provides the attention contract.
pub fn is_attention_material(fact: &InstalledUnitFact) -> bool {
    fact.kind == UnitKind::Awareness
        || fact.kind == UnitKind::Perception
        || CapabilityContract {
            contract_id: ATTENTION_CONTRACT.to_string(),
            major: HOST_CONTRACT_MAJOR,
        }
        .declared_by(&fact.descriptor.provides)
}

/// Digest of the atom's own content, for a caller that wants to bind an
/// adoption or an export to it without re-reading the authority.
pub fn content_digest(unit: &CognitionUnit) -> Digest {
    Digest(unit.artifact_digest.clone())
}

/// The adoption authority the host consults before one content revision may
/// take part in the capability.
pub trait AtomAdoption: Send + Sync + 'static {
    /// Whether this exact installed revision carries an authenticated adoption
    /// attitude on this installation.
    fn adopted(&self, unit: &CognitionUnit) -> bool;
}

/// The installation authority's own authenticated adoption.
///
/// The Node records the decision, verifies every cited piece of evidence
/// against its own durable state and binds the decision to the exact artifact
/// identity. That is enough for attributed learning from external content: an
/// imported or merely read unit is never adopted by being present, and nothing
/// here requires that this installation ran an empirical trial itself. A
/// foreign execution receipt stays foreign provenance — it is not a local
/// receipt, and it never becomes one by being cited.
pub struct InstallationAdoption;

impl AtomAdoption for InstallationAdoption {
    fn adopted(&self, unit: &CognitionUnit) -> bool {
        adopted_here(unit)
    }
}

/// The awareness material this distribution ships: one installable capability
/// provider and one atomic attention material.
///
/// These are ordinary packages installed through the same authority as every
/// other unit. Nothing here is privileged beyond its provenance, and installing
/// the provider still requires the installation's own adoption attitude before
/// the host reports itself resident.
pub fn awareness_seed_packages() -> Result<Vec<CognitionPackage>> {
    const SEEDS: &[(&str, &str)] = &[
        (
            "seed-awareness/host/HOST.md",
            include_str!("../seed-awareness/host/HOST.md"),
        ),
        (
            "seed-awareness/atoms/foundation-cognition/AWARENESS.md",
            include_str!("../seed-awareness/atoms/foundation-cognition/AWARENESS.md"),
        ),
    ];
    SEEDS
        .iter()
        .map(|(path, content)| parse_cognition_entry(content, Path::new(path)))
        .collect()
}
