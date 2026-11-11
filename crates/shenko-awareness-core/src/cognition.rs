//! Installable cognition units — docs/specs/cognition-units.md#spec-45-1–docs/specs/cognition-units.md#spec-45-10.
//!
//! Perception, awareness, Skill and code are one kind of installable unit.
//! They share one installation authority: the Node's unit store. This module
//! owns what a unit *contains* — the minimal package form, its capability
//! card, its provenance and its learning record — and projects the durable
//! installation facts into the short index the model reads.
//!
//! There is no second registry. [`CognitionCatalog`] is a read model over the
//! Node's units; every write goes back through the Node, so version, enable
//! state, dependencies, supersession and adoption have exactly one owner.
//!
//! What the catalog deliberately does not do: it never decides which unit is
//! relevant to a message. A symptom names a symptom, not the concept that
//! would explain it, so a keyword gate hides exactly the perspective that was
//! meant to help. The model receives the short cards and reads the body of
//! the unit it actually wants, at the exact version it read.

use std::borrow::Cow;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use chrono::{DateTime, Utc};
use cordis_core::{Context, Plugin, Service};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_node::{
    AdoptionDecision, AdoptionEvidence, AdoptionMethod, Applicability, DependencySpec, Node,
    UnitDescriptor, UnitExport, UnitKind, UnitPackage, UnitRequest, UNIT_SCHEMA,
};
use shenko_types::object::Digest;

use crate::perception::{
    PerceptionBinding, PerceptionRegistry, PerceptionStatus, reconcile_perception_sources_from,
};

/// Maximum bytes of one capability card.
pub const MAX_CARD_BYTES: usize = 512;
/// Maximum bytes of the disclosed index of every installed cognition unit.
pub const MAX_INDEX_BYTES: usize = 4096;
/// Maximum number of cards one discovery page carries.
pub const MAX_PAGE_CARDS: usize = 32;
/// Maximum bytes of one unit body returned by an exact-version read.
pub const MAX_BODY_BYTES: usize = 16 * 1024;
/// Maximum bytes of one authored package entry.
pub const MAX_ENTRY_BYTES: usize = 64 * 1024;

/// The native tool a reader calls to fetch one unit body at an exact version.
pub const READ_TOOL: &str = "cognition.unit.read";
/// The native tool a reader calls for one page of the discovery index.
pub const LIST_TOOL: &str = "cognition.unit.list";
/// The definition version of both native tools in this build.
pub const TOOL_VERSION: u32 = 1;

/// How a reader addresses the material behind the index.
///
/// The main subject reads a body by delegating a native task; a finite child
/// calls the native tool its own delegation granted. The index itself is the
/// same installed catalogue either way — main and children share every
/// discoverable unit, while the role and the grant decide what each of them
/// can execute (docs/specs/cognition-units.md#spec-45-4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CognitionCall {
    /// The subject's own delegation syntax.
    Delegate,
    /// A finite child's granted native tools. Both grants travel separately:
    /// a delegation may grant the exact-version read, the paginated index, or
    /// only the index. The disclosure then never advertises a call the child
    /// cannot make.
    Tool { read: bool, list: bool },
}

impl CognitionCall {
    /// Whether this reader can fetch one unit body at an exact version.
    pub fn can_read(&self) -> bool {
        match self {
            CognitionCall::Delegate => true,
            CognitionCall::Tool { read, .. } => *read,
        }
    }

    /// Whether this reader can page the installed index.
    pub fn can_list(&self) -> bool {
        match self {
            CognitionCall::Delegate => true,
            CognitionCall::Tool { list, .. } => *list,
        }
    }

    /// Whether this reader can make any call the index would name.
    pub fn is_empty(&self) -> bool {
        !self.can_read() && !self.can_list()
    }
}

// ------------------------------------------------------------- metadata ---

/// The state a cognition unit is offered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CognitionState {
    /// Installed, current and enabled.
    Enabled,
    /// Installed, current but switched off.
    Disabled,
    /// Installed and recorded, but not yet judged good enough to offer.
    Candidate,
    /// Installed, enabled and backed by a measurement this installation ran
    /// itself. An enabled unit without such a measurement stays `Enabled`:
    /// being switched on is not being measured.
    Adopted,
    /// Judged and kept out of use; the reason and the evidence stay attached.
    Rejected,
    /// Kept as history but no longer offered.
    Retired,
    /// Replaced by a later version.
    Superseded,
}

impl CognitionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            CognitionState::Enabled => "enabled",
            CognitionState::Disabled => "disabled",
            CognitionState::Candidate => "candidate",
            CognitionState::Adopted => "adopted",
            CognitionState::Rejected => "rejected",
            CognitionState::Retired => "retired",
            CognitionState::Superseded => "superseded",
        }
    }
}

/// Where a learned unit came from.
///
/// The reference names an actual observation. The kernel records the claim and
/// whether it could confirm it; it never decides what the observation means.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LearningRecord {
    /// The kind of the observation: `inbox_event`, `cas_object`, `tool_result`,
    /// `session_event`, `external_text`.
    pub source_kind: String,
    /// The reference of that observation.
    pub source_ref: String,
    /// The digest of the observed bytes, when the source has one.
    pub source_digest: Option<String>,
    /// Whether the kernel could confirm the reference against a real fact.
    pub source_verified: bool,
    /// How the reference was confirmed: `store_cas` for a sealed object in
    /// this installation's content store, `node_observation` for a
    /// measurement this installation registered, `claimed` when nothing could
    /// confirm it. A claimed source keeps the unit a candidate.
    pub verified_by: String,
    /// When the unit was written down.
    pub learned_at: String,
    /// The learning state: `candidate` until the subject judges it.
    pub state: String,
}

/// One version a cognition unit declares it replaces.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SupersedesRef {
    pub revision: String,
    pub artifact_digest: String,
    pub reason: Option<String>,
}

/// How one version was judged on this installation.
///
/// The record is this installation's attitude towards one immutable content
/// revision: the artifact digest it names, the reason pointers it cites and
/// how far that reason reaches. It carries the subject's own confidence;
/// `None` is "not judged yet" and is never replaced by a default number.
///
/// It is the domain view of the attitude, read from the unit's own
/// configuration. The canonical content/attitude/confidence type is owned
/// elsewhere; this record delegates to it rather than opening a second store.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AdoptionRecord {
    pub decision: String,
    pub method: String,
    /// The evidence the node verified. A trial attempt is a runner-owned
    /// record of a trial this installation ran; an observation is bytes this
    /// installation holds.
    pub evidence: Vec<AdoptionEvidenceFact>,
    /// Whether every cited evidence item resolved against this installation.
    pub evidence_verified: bool,
    /// Whether the evidence carries a measurement. Only a trial attempt that
    /// recorded one can.
    pub empirical: bool,
    /// What the cited trials can carry, classified by the node that recorded
    /// them: `measured`, `partial`, `inconclusive` or `unexecuted`.
    ///
    /// It is absent on records written before the node classified evidence.
    /// Such a record's `empirical` flag is the judgement that was recorded at
    /// the time and is never rewritten here; its evidence was not classified,
    /// so nothing in this projection claims it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measurement: Option<String>,
    /// The subject's stated confidence in this judgement.
    ///
    /// Three states, because they are different facts: the key is absent on a
    /// record written before this installation recorded confidence at all;
    /// `null` means the subject explicitly stated none, which is recorded as
    /// unknown rather than a filled-in default; a number is what it stated.
    /// It maps onto the canonical attitude confidence as `None` /
    /// `Some(Unknown)` / `Some(Stated { value })`.
    #[serde(
        default,
        deserialize_with = "claimed_confidence",
        skip_serializing_if = "Option::is_none"
    )]
    pub confidence: Option<Option<f64>>,
    pub notes: Option<String>,
    pub recorded_at: String,
}

impl AdoptionRecord {
    /// The confidence the subject stated, when it stated one.
    ///
    /// Callers ask this instead of unwrapping the recorded shape: an absent
    /// value and an explicit unknown both mean "no number", and only a stated
    /// number is a reading of the subject's own judgement.
    pub fn stated_confidence(&self) -> Option<f64> {
        self.confidence.flatten()
    }
}

/// Read the recorded confidence without collapsing "absent" into "unknown".
///
/// A value this reading cannot use is treated as "no number stated" rather
/// than as a parse failure: losing the whole recorded attitude because one
/// field is unrecognisable would rewrite the judgement by omission.
fn claimed_confidence<'de, D>(deserializer: D) -> Result<Option<Option<f64>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let recorded = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(Some(match recorded {
        Some(value) => value.as_f64().filter(|value| value.is_finite()),
        None => None,
    }))
}

/// One verified piece of evidence behind a recorded judgement.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AdoptionEvidenceFact {
    pub kind: String,
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_digest: Option<String>,
    /// What this cited attempt recorded, when the node classified it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measurement: Option<String>,
    /// The terminal outcome of the cited attempt's trial row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// The environment the cited run was bound to. A command trial has no
    /// plan, so this is what binds it to the machine it ran on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_digest: Option<String>,
    /// The receipts the cited run wrote, and how many were judge-scored
    /// facts of a slot its own plan named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipts: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scored: Option<u64>,
    /// The sample slots the attempt's own plan named, when it names them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_slots: Option<u64>,
}

/// The metadata a cognition unit carries next to its body.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CognitionMetadata {
    pub title: String,
    pub capability_card: String,
    pub why: Option<String>,
    pub cues: Vec<String>,
    pub negative_cues: Vec<String>,
    pub applicability: Option<String>,
    pub knowledge: Vec<String>,
    pub provenance_refs: Vec<String>,
    pub learning: Option<LearningRecord>,
    pub supersedes: Vec<SupersedesRef>,
}

// -------------------------------------------------------------- package ---

/// One authored cognition unit, before it is installed.
///
/// The authored form is the minimal package of docs/specs/cognition-units.md#spec-45-2: an entry file with
/// a small frontmatter and a body. The body is the material the model reads;
/// the frontmatter carries the identity, the card and the provenance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CognitionPackage {
    pub unit_id: String,
    pub kind: UnitKind,
    pub revision: String,
    pub title: String,
    pub capability_card: String,
    pub why: Option<String>,
    pub cues: Vec<String>,
    pub negative_cues: Vec<String>,
    pub applicability: Option<String>,
    pub knowledge: Vec<String>,
    pub provenance_refs: Vec<String>,
    pub learning: Option<LearningRecord>,
    pub supersedes: Vec<SupersedesRef>,
    /// Capability and topic names this unit declares it provides. They are
    /// what an abstract dependency resolves against.
    pub provides: Vec<String>,
    /// Declared dependencies on other units.
    pub dependencies: Vec<DependencySpec>,
    /// The intuition body: the material the model reads.
    pub body: String,
    /// The whole authored entry, frontmatter included. This is the artifact
    /// the unit stores, so the installed content stays self-describing.
    pub entry_text: String,
    /// Whether the unit is offered after installation.
    pub enabled: bool,
}

impl CognitionPackage {
    /// The authored entry bytes of this package.
    pub fn entry_bytes(&self) -> Vec<u8> {
        self.entry_text.as_bytes().to_vec()
    }

    /// The metadata of this package.
    pub fn metadata(&self) -> CognitionMetadata {
        CognitionMetadata {
            title: self.title.clone(),
            capability_card: self.capability_card.clone(),
            why: self.why.clone(),
            cues: self.cues.clone(),
            negative_cues: self.negative_cues.clone(),
            applicability: self.applicability.clone(),
            knowledge: self.knowledge.clone(),
            provenance_refs: self.provenance_refs.clone(),
            learning: self.learning.clone(),
            supersedes: self.supersedes.clone(),
        }
    }

    /// The unit descriptor that carries this package's metadata.
    pub fn descriptor(&self, artifact_digest: Digest) -> UnitDescriptor {
        UnitDescriptor {
            unit_id: self.unit_id.clone(),
            kind: self.kind,
            revision: self.revision.clone(),
            artifact_digest,
            provides: self.provides.clone(),
            dependencies: self.dependencies.clone(),
            config: json!({ "cognition": serde_json::to_value(self.metadata()).unwrap_or(Value::Null) }),
            applicability: self.applicability.clone().map(|notes| Applicability {
                os: None,
                arch: None,
                runtime: None,
                notes: Some(notes),
            }),
        }
    }

    /// The transferable package of this unit.
    pub fn export(&self) -> Result<(UnitExport, Digest)> {
        let bytes = self.entry_bytes();
        let digest = Digest::blake3(&bytes);
        let export = UnitExport {
            schema: UNIT_SCHEMA,
            root_unit_id: self.unit_id.clone(),
            root_revision: Some(self.revision.clone()),
            root_artifact_digest: Some(digest.clone()),
            origin_node: None,
            enabled: self.enabled,
            packages: vec![UnitPackage {
                unit: self.descriptor(digest.clone()),
                artifact_base64: shenko_node::b64_encode(&bytes),
                evidence: vec![],
                resolved_dependencies: None,
            }],
        };
        Ok((export, digest))
    }
}

/// Parse one authored cognition entry.
///
/// The frontmatter is the minimal form of docs/specs/cognition-units.md#spec-45-2: `key: value` or
/// `key: [a, b]` lines between `---` markers. Everything after the closing
/// marker is the body.
pub fn parse_cognition_entry(content: &str, entry_path: &Path) -> Result<CognitionPackage> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    ensure!(
        content.len() <= MAX_ENTRY_BYTES,
        "cognition entry {} exceeds the size bound",
        entry_path.display()
    );
    let (frontmatter, body) = split_frontmatter(content, entry_path)?;
    let unit_id = frontmatter
        .get("id")
        .cloned()
        .with_context(|| format!("{} has no `id` in frontmatter", entry_path.display()))?;
    let revision = frontmatter
        .get("version")
        .cloned()
        .with_context(|| format!("{} has no `version` in frontmatter", entry_path.display()))?;
    let kind = match frontmatter.get("kind").map(String::as_str) {
        Some("perception") => UnitKind::Perception,
        Some("awareness") | None => UnitKind::Awareness,
        Some("skill") => UnitKind::Skill,
        Some("code") => UnitKind::Code,
        Some(other) => anyhow::bail!("{} has an unknown unit kind '{other}'", entry_path.display()),
    };
    ensure!(!body.is_empty(), "{} has no body after the frontmatter", entry_path.display());
    let capability_card = frontmatter
        .get("capability_card")
        .cloned()
        .or_else(|| first_heading(&body))
        .unwrap_or_else(|| unit_id.clone());
    let learning = match frontmatter.get("learning_source_ref") {
        Some(source_ref) => Some(LearningRecord {
            source_kind: frontmatter
                .get("learning_source_kind")
                .cloned()
                .unwrap_or_else(|| "external_text".to_string()),
            source_ref: source_ref.clone(),
            source_digest: frontmatter.get("learning_source_digest").cloned(),
            source_verified: false,
            verified_by: "claimed".to_string(),
            learned_at: chrono::Utc::now().to_rfc3339(),
            state: frontmatter
                .get("learning_state")
                .cloned()
                .unwrap_or_else(|| "candidate".to_string()),
        }),
        None => None,
    };
    let supersedes = frontmatter
        .get("supersedes")
        .map(|raw| parse_supersedes(raw, entry_path))
        .transpose()?
        .unwrap_or_default();
    Ok(CognitionPackage {
        unit_id,
        kind,
        revision,
        title: frontmatter
            .get("title")
            .cloned()
            .unwrap_or_else(|| capability_card.clone()),
        capability_card,
        why: frontmatter.get("why").cloned(),
        cues: list_field(&frontmatter, &["cues"]),
        negative_cues: list_field(&frontmatter, &["negative_cues", "negative_cues"]),
        applicability: frontmatter.get("applicability").cloned(),
        knowledge: list_field(&frontmatter, &["knowledge"]),
        provenance_refs: list_field(&frontmatter, &["provenance", "provenance_refs"]),
        learning,
        supersedes,
        provides: list_field(&frontmatter, &["provides"]),
        dependencies: Vec::new(),
        body,
        entry_text: content.to_string(),
        enabled: frontmatter
            .get("enabled")
            .map(|value| value != "false" && value != "no")
            .unwrap_or(true),
    })
}

/// The built-in seed units shipped with this installation.
///
/// The seeds are ordinary units: they are installed into the same authority
/// as every other unit and carry the same version, enable and provenance
/// facts. Nothing about them is privileged beyond their provenance.
pub fn seed_packages() -> Result<Vec<CognitionPackage>> {
    const SEEDS: &[(&str, &str)] = &[
        (
            "seed-awareness/network-change/AWARENESS.md",
            include_str!("../seed-awareness/network-change/AWARENESS.md"),
        ),
        (
            "seed-awareness/capability-gap/AWARENESS.md",
            include_str!("../seed-awareness/capability-gap/AWARENESS.md"),
        ),
        (
            "seed-awareness/general-principles/AWARENESS.md",
            include_str!("../seed-awareness/general-principles/AWARENESS.md"),
        ),
        (
            "seed-perception/installation-units/PERCEPTION.md",
            include_str!("../seed-perception/installation-units/PERCEPTION.md"),
        ),
        (
            "seed-awareness/subject-frame/AWARENESS.md",
            include_str!("../seed-awareness/subject-frame/AWARENESS.md"),
        ),
        (
            "seed-awareness/idle-perception/AWARENESS.md",
            include_str!("../seed-awareness/idle-perception/AWARENESS.md"),
        ),
        (
            "seed-awareness/subject-attention/AWARENESS.md",
            include_str!("../seed-awareness/subject-attention/AWARENESS.md"),
        ),
        (
            "seed-awareness/subject-child-frame/AWARENESS.md",
            include_str!("../seed-awareness/subject-child-frame/AWARENESS.md"),
        ),
    ];
    SEEDS
        .iter()
        .map(|(path, content)| parse_cognition_entry(content, Path::new(path)))
        .collect()
}

// ------------------------------------------------------- capabilities ----

/// The capability contract the subject's own role frame is resolved
/// against. docs/specs/subject-runtime.md#spec-57-1: the frame is a content unit the subject may revise,
/// so the harness asks for a capability instead of naming a package.
pub const CAP_SUBJECT_FRAME: &str = "cap.subject-frame";

/// The capability contract for what the subject receives when no new
/// external input arrived. docs/specs/subject-runtime.md#spec-57-2: idleness is a perception, not a
/// command line.
pub const CAP_IDLE_PERCEPTION: &str = "cap.idle-perception";

/// The capability contract for the role frame a delegated child runs under.
///
/// docs/specs/self-update.md#spec-57-10: the frame says what kind of worker the child is. It is the
/// subject's to write — it can give different tasks different children — while
/// the harness keeps the real protocol boundaries (the tool catalog, the reply
/// grant, the budget).
pub const CAP_CHILD_ROLE_FRAME: &str = "cap.child-role-frame";

/// The capability contract for the subject's own attention budget.
///
/// docs/specs/subject-runtime.md#spec-57-3: how many inputs one step reads, and how long the subject
/// waits for more, are the subject's setting — not a constant the harness
/// picked. The unit that provides this contract states the numbers; the
/// harness keeps only the bounds that prevent starvation.
pub const CAP_ATTENTION_POLICY: &str = "cap.attention-policy";

/// The resolution of one capability contract against the installation.
///
/// `Absent` means nothing installed provides it and the compiled fallback
/// applies. `Ambiguous` means two or more DISTINCT units provide it: the
/// harness does not pick one silently, it keeps the compiled fallback and
/// tells the subject, because which frame is in force is the subject's own
/// decision (docs/specs/cognition-units.md#spec-51-8, docs/specs/subject-runtime.md#spec-57-1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityUnit {
    /// Exactly one unit provides the capability; this is its current body.
    Installed {
        unit_id: String,
        revision: String,
        body: String,
    },
    /// Nothing installed provides the capability.
    Absent,
    /// More than one distinct unit provides the capability.
    Ambiguous { unit_ids: Vec<String> },
}

impl CapabilityUnit {
    /// The installed body, when exactly one unit provides the capability.
    pub fn body(&self) -> Option<&str> {
        match self {
            CapabilityUnit::Installed { body, .. } => Some(body.as_str()),
            _ => None,
        }
    }

    /// Whether the resolution is ambiguous between distinct units.
    pub fn is_ambiguous(&self) -> bool {
        matches!(self, CapabilityUnit::Ambiguous { .. })
    }
}

impl CognitionCatalog {
    /// Resolve one capability contract against the installed units.
    ///
    /// Only enabled units count: a unit the subject retired is not a
    /// provider. The current revision of the single providing unit is
    /// returned; several DISTINCT unit ids are an ambiguity, not a race.
    pub fn capability_unit(&self, capability: &str) -> Result<CapabilityUnit> {
        let installed = self.units()?;
        let mut providers: Vec<&CognitionUnit> = installed
            .iter()
            .filter(|unit| unit.enabled && unit.provides.iter().any(|name| name == capability))
            .collect();
        providers.sort_by(|left, right| left.unit_id.cmp(&right.unit_id));
        match providers.len() {
            0 => Ok(CapabilityUnit::Absent),
            1 => {
                let unit = providers[0];
                let read = self.read(&unit.unit_id, &unit.revision)?;
                Ok(CapabilityUnit::Installed {
                    unit_id: unit.unit_id.clone(),
                    revision: unit.revision.clone(),
                    body: read.body,
                })
            }
            _ => Ok(CapabilityUnit::Ambiguous {
                unit_ids: providers
                    .into_iter()
                    .map(|unit| unit.unit_id.clone())
                    .collect(),
            }),
        }
    }
}

// --------------------------------------------------------------- facts ----

/// One installed dependency edge.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DependencyFact {
    pub edge_id: String,
    pub strength: String,
    pub status: String,
    pub selected_unit_id: Option<String>,
    pub selected_revision: Option<String>,
    pub selected_artifact_digest: Option<String>,
    pub reason: String,
}

/// The durable facts of one installed cognition unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitionUnit {
    pub unit_id: String,
    pub kind: UnitKind,
    pub revision: String,
    pub artifact_digest: String,
    pub current: bool,
    pub enabled: bool,
    pub state: CognitionState,
    pub title: String,
    pub capability_card: String,
    pub why: Option<String>,
    pub cues: Vec<String>,
    pub negative_cues: Vec<String>,
    pub applicability: Option<String>,
    pub knowledge: Vec<String>,
    pub provenance_refs: Vec<String>,
    pub learning: Option<LearningRecord>,
    pub adoption: Option<AdoptionRecord>,
    pub supersedes: Vec<SupersedesRef>,
    pub superseded_by: Vec<SupersedesRef>,
    pub provides: Vec<String>,
    pub dependencies: Vec<DependencyFact>,
    pub imported_at: String,
    pub origin_node: Option<String>,
}

/// The short index entry of one installed cognition unit.
///
/// A card states installation facts only. The call that fetches the body is a
/// property of the reader — the subject delegates, a child calls the tool its
/// own delegation granted — so the index names it per reader instead of
/// freezing one role's syntax into the shared fact.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitionCard {
    pub unit_id: String,
    pub kind: UnitKind,
    pub revision: String,
    pub artifact_digest: String,
    pub title: String,
    pub capability_card: String,
    pub cues: Vec<String>,
    pub negative_cues: Vec<String>,
    pub applicability: Option<String>,
    pub state: CognitionState,
    pub adoption: Option<String>,
    /// Whether a measurement this installation ran backs this unit.
    pub empirical: bool,
    /// What the evidence behind the recorded judgement can carry, when the
    /// node classified it: `measured`, `partial`, `inconclusive` or
    /// `unexecuted`. Absent when the version carries no classified evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measurement: Option<String>,
    /// What this node has actually observed for this perception unit version.
    ///
    /// Only perception units carry one: an awareness, Skill or code unit is
    /// material, not an observation. `unknown` and `unavailable` are real
    /// states and neither means the thing being watched is fine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perception: Option<PerceptionStatus>,
}

/// The content of one unit at one exact version.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitionRead {
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: String,
    pub kind: UnitKind,
    pub enabled: bool,
    pub current: bool,
    pub title: String,
    pub capability_card: String,
    /// The intuition body, without the frontmatter.
    pub body: String,
    pub body_truncated: bool,
    /// The whole authored entry, frontmatter included.
    pub entry_text: String,
    pub entry_truncated: bool,
    pub provenance_refs: Vec<String>,
    pub learning: Option<LearningRecord>,
    pub adoption: Option<AdoptionRecord>,
    pub supersedes: Vec<SupersedesRef>,
    pub superseded_by: Vec<SupersedesRef>,
    pub dependencies: Vec<DependencyFact>,
}

/// A fresh view of the installation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogSnapshot {
    /// Increases on every refresh; a caller that holds an older snapshot can
    /// tell that the installation moved on.
    pub generation: u64,
    pub units: Vec<CognitionUnit>,
}

/// One bounded page of the discovery index.
///
/// The revision identifies the installation the page was cut from. A caller
/// that holds a page from an older installation must ask again: mixing pages
/// of two installations would show units that no longer exist and hide units
/// that do.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitionDiscovery {
    /// The installation revision this page was cut from.
    pub catalog_revision: u64,
    /// How many units the installation offers in total.
    pub total: usize,
    /// The index of the first card in this page.
    pub start: usize,
    /// How many cards this page carries.
    pub count: usize,
    /// The index of the first card that is not in this page.
    pub next_start: Option<usize>,
    /// The cards of this page.
    pub cards: Vec<CognitionCard>,
}

/// One observation registered against a unit version.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitionObservation {
    pub digest: String,
    pub kind: String,
    pub bytes: u64,
    pub recorded_at: String,
    /// Whether this installation still holds the observation bytes.
    pub bytes_held: bool,
}

// -------------------------------------------------------------- catalog ----

/// The read model over the installation authority.
///
/// The catalog owns no installation state of its own: it reads the Node's
/// units and writes back through the Node. A refresh re-reads the authority
/// and reports the new generation.
pub struct CognitionCatalog {
    node: Arc<Mutex<Node>>,
    generation: RwLock<u64>,
    /// The live half of the same catalogue: which source speaks for which
    /// exact installed perception revision, and what it last observed. It
    /// holds no installation state — a source is bound only to a revision the
    /// Node actually holds.
    perception: PerceptionRegistry,
}

impl CognitionCatalog {
    /// Wrap the installation authority this instance already owns.
    pub fn new(node: Arc<Mutex<Node>>) -> Self {
        Self {
            node,
            generation: RwLock::new(0),
            perception: PerceptionRegistry::new(),
        }
    }

    /// The observation sources bound to exact installed perception revisions.
    pub fn perception(&self) -> &PerceptionRegistry {
        &self.perception
    }

    /// The current generation of the read model.
    pub fn generation(&self) -> u64 {
        *self.generation.read()
    }

    /// The revision of the installation this view reads.
    ///
    /// The revision is derived from the installation facts themselves, so two
    /// processes that see the same installation agree on it and any change to
    /// the installation changes it. A page of the discovery index carries the
    /// revision it was cut from; a caller that sees a different revision
    /// knows the page is stale.
    pub fn catalog_revision(&self) -> Result<u64> {
        let facts = self.facts()?;
        revision_of(&facts)
    }

    /// Re-read the installation authority.
    pub fn refresh(&self) -> Result<CatalogSnapshot> {
        let units = self.units()?;
        let mut generation = self.generation.write();
        *generation += 1;
        Ok(CatalogSnapshot {
            generation: *generation,
            units,
        })
    }

    /// Every installed unit, current versions only.
    pub fn units(&self) -> Result<Vec<CognitionUnit>> {
        let facts = self.facts()?;
        Ok(facts.iter().map(unit_from_fact).collect())
    }

    /// Every retained version of one unit, newest relation first.
    pub fn versions(&self, unit_id: &str) -> Result<Vec<CognitionUnit>> {
        let node = self.node.lock();
        let facts = node.retained_unit_facts(unit_id)?;
        Ok(facts.iter().map(unit_from_fact).collect())
    }

    /// One reading of the installation authority.
    ///
    /// A projection derives its cards, its revision and its observation
    /// bindings from one reading, so the index can never mix units of two
    /// installations or answer with a source bound to a version that reading
    /// no longer contains.
    fn facts(&self) -> Result<Vec<shenko_node::InstalledUnitFact>> {
        Ok(self.node.lock().installed_unit_facts()?)
    }

    /// The short index of the installed cognition units.
    ///
    /// The index carries the card of every unit that authored one. A unit
    /// without a card is still installed and still readable; it simply has
    /// nothing to disclose before it is read.
    pub fn cards(&self) -> Result<Vec<CognitionCard>> {
        let facts = self.facts()?;
        self.cards_from(&facts)
    }

    /// The cards of one reading of the installation.
    fn cards_from(
        &self,
        facts: &[shenko_node::InstalledUnitFact],
    ) -> Result<Vec<CognitionCard>> {
        // The sources are re-derived from the installation on every
        // projection: an updated or removed unit must not keep answering
        // through the binding of the version it replaced.
        reconcile_perception_sources_from(facts, &self.node, &self.perception)?;
        Ok(facts
            .iter()
            .map(unit_from_fact)
            .filter(|unit| {
                !unit.capability_card.is_empty()
                    && matches!(
                        unit.state,
                        CognitionState::Enabled
                            | CognitionState::Candidate
                            | CognitionState::Adopted
                    )
            })
            .map(|unit| {
                // A perception unit's card carries the observation this node
                // holds for this exact version. Reading the definition does
                // not produce one, so an unbound or silent source is reported
                // as `unknown` rather than as a healthy fact.
                let perception = (unit.kind == UnitKind::Perception).then(|| {
                    self.perception.observe(&PerceptionBinding {
                        unit_id: unit.unit_id.clone(),
                        revision: unit.revision.clone(),
                        artifact_digest: unit.artifact_digest.clone(),
                    })
                });
                CognitionCard {
                    unit_id: unit.unit_id.clone(),
                    kind: unit.kind,
                    revision: unit.revision.clone(),
                    artifact_digest: unit.artifact_digest.clone(),
                    title: unit.title.clone(),
                    capability_card: unit.capability_card.clone(),
                    cues: unit.cues.clone(),
                    negative_cues: unit.negative_cues.clone(),
                    applicability: unit.applicability.clone(),
                    state: unit.state,
                    adoption: unit.adoption.as_ref().map(|adoption| adoption.decision.clone()),
                    empirical: unit
                        .adoption
                        .as_ref()
                        .is_some_and(|adoption| adoption.empirical && adoption.evidence_verified),
                    measurement: unit
                        .adoption
                        .as_ref()
                        .and_then(|adoption| adoption.measurement.clone()),
                    perception,
                }
            })
            .collect())
    }

    /// The bounded index text disclosed to the model.
    ///
    /// Only the cards travel. The body of a unit is read on demand at its
    /// exact version, so the disclosure stays small and every unit stays
    /// discoverable even when the library grows: the text never grows past
    /// [`MAX_INDEX_BYTES`], and the cards that did not fit are reached
    /// through the paginated index the last line names.
    pub fn disclosure(&self) -> Result<String> {
        self.render_index(Some(CognitionCall::Delegate))
    }

    /// The same bounded index, rendered for one reader.
    ///
    /// `call` names the form that fetches a body for this reader. `None` means
    /// this reader may not fetch bodies this turn: the index still says what
    /// is installed — main and children share every discoverable unit — but
    /// it does not advertise a call the reader cannot make.
    pub fn render_index(&self, call: Option<CognitionCall>) -> Result<String> {
        let facts = self.facts()?;
        let revision = revision_of(&facts)?;
        let cards = self.cards_from(&facts)?;
        let total = cards.len();
        // The footer names the total, the revision and the exact next call.
        // Its size is bounded by construction, so reserving room for it keeps
        // the whole disclosure inside the bound.
        const FOOTER_RESERVE: usize = 320;
        let budget = MAX_INDEX_BYTES.saturating_sub(FOOTER_RESERVE);
        let now = chrono::Utc::now();
        let mut lines: Vec<String> = Vec::new();
        let mut used = 0_usize;
        for card in &cards {
            let line = card_line(card, call, now);
            if used + line.len() + 1 > budget {
                break;
            }
            used += line.len();
            lines.push(line);
        }
        // A page past the end has no reader-independent continuation: the
        // footer only offers one when this reader can actually make the call.
        let footer = |shown: usize| -> String {
            if shown >= total {
                return format!("（共 {total} 个单元，已全部显示；目录修订 {revision}）\n");
            }
            match call.and_then(|call| list_op_for(call, shown, MAX_PAGE_CARDS, revision)) {
                Some(op) => format!(
                    "（共 {total} 个单元，此处显示 {shown} 个；目录修订 {revision}；其余按需分页读取：{op}）\n"
                ),
                None => format!(
                    "（共 {total} 个单元，此处显示 {shown} 个；目录修订 {revision}；其余本轮不可分页读取）\n"
                ),
            }
        };
        let mut shown = lines.len();
        let mut text = String::new();
        for line in &lines {
            text.push_str(line);
            text.push('\n');
        }
        text.push_str(&footer(shown));
        if text.len() > MAX_INDEX_BYTES {
            // The reserve is generous, but the bound is the contract: drop
            // whole cards until the text fits rather than truncating one.
            while text.len() > MAX_INDEX_BYTES && !lines.is_empty() {
                lines.pop();
                shown -= 1;
                text.clear();
                for line in &lines {
                    text.push_str(line);
                    text.push('\n');
                }
                text.push_str(&footer(shown));
            }
        }
        Ok(text)
    }

    /// One bounded page of the discovery index.
    ///
    /// The page is cut from the current installation and carries its
    /// revision. `start` is the index of the first card, so a caller that
    /// continues where the disclosure stopped never skips a card and never
    /// sees one twice.
    pub fn discovery(&self, start: usize, count: usize) -> Result<CognitionDiscovery> {
        let count = count.clamp(1, MAX_PAGE_CARDS);
        let facts = self.facts()?;
        let revision = revision_of(&facts)?;
        let cards = self.cards_from(&facts)?;
        let total = cards.len();
        let start = start.min(total);
        let end = start.saturating_add(count).min(total);
        let page = cards[start..end].to_vec();
        let next_start = (end < total).then_some(end);
        Ok(CognitionDiscovery {
            catalog_revision: revision,
            total,
            start,
            count: page.len(),
            next_start,
            cards: page,
        })
    }

    /// Read one unit's content at one exact version.
    pub fn read(&self, unit_id: &str, revision: &str) -> Result<CognitionRead> {
        let node = self.node.lock();
        let facts = node.retained_unit_facts(unit_id)?;
        let fact = facts
            .iter()
            .find(|fact| fact.revision == revision)
            .with_context(|| format!("unit '{unit_id}' has no retained revision '{revision}'"))?;
        let content = node.unit_content(unit_id, revision, None)?;
        let metadata = metadata_from_config(&fact.descriptor.config);
        let entry_text = String::from_utf8_lossy(&content.bytes).to_string();
        let body = strip_frontmatter(&entry_text);
        let (body, body_truncated) = truncate_utf8(&body, MAX_BODY_BYTES);
        let (entry_text, entry_truncated) = truncate_utf8(&entry_text, MAX_BODY_BYTES * 2);
        Ok(CognitionRead {
            unit_id: unit_id.to_string(),
            revision: revision.to_string(),
            artifact_digest: content.artifact_digest.0.clone(),
            kind: content.kind,
            enabled: content.enabled,
            current: content.current,
            title: metadata.title.clone(),
            capability_card: metadata.capability_card.clone(),
            body,
            body_truncated,
            entry_text,
            entry_truncated,
            provenance_refs: metadata.provenance_refs.clone(),
            learning: metadata.learning.clone(),
            adoption: adoption_from_config(&fact.descriptor.config),
            supersedes: metadata.supersedes.clone(),
            superseded_by: supersedes_from_fact(fact),
            dependencies: dependencies_of(fact),
        })
    }

    /// Export the current unit's authored descriptor, bytes and available
    /// dependency closure through the installation authority.
    pub fn export(&self, unit_id: &str) -> Result<UnitExport> {
        let mut response = self.node.lock().handle_unit_request(UnitRequest::Export {
            unit_id: unit_id.to_owned(),
        })?;
        serde_json::from_value(response.get_mut("export")
            .context("the installation export is missing its package")?.take())
            .context("the installation returned an invalid package export")
    }

    /// Install one authored unit into the installation authority.
    pub fn install(&self, package: &CognitionPackage, package_root: &Path) -> Result<Value> {
        let (export, digest) = package.export()?;
        let mut node = self.node.lock();
        let outcome = node.handle_unit_request(UnitRequest::Import {
            export,
            package_root: package_root.to_string_lossy().into_owned(),
            enable: Some(package.enabled),
        })?;
        Ok(json!({
            "unit_id": package.unit_id,
            "revision": package.revision,
            "artifact_digest": digest,
            "outcome": outcome,
        }))
    }

    /// Switch one unit on or off on this installation.
    pub fn set_enabled(&self, unit_id: &str, enabled: bool) -> Result<Value> {
        let mut node = self.node.lock();
        node.handle_unit_request(UnitRequest::SetEnabled {
            unit_id: unit_id.to_string(),
            enabled,
        })
    }

    /// Record how one version was judged, and on which evidence.
    ///
    /// The node verifies every evidence item against its own durable state.
    /// A trial attempt is a record of a trial this installation ran; a stored
    /// observation is bytes this installation holds. Only the first can carry
    /// empirical acceptance. `confidence` is what the subject stated about
    /// its own judgement; `None` records that it has not judged yet.
    pub fn record_adoption(
        &self,
        unit_id: &str,
        revision: &str,
        artifact_digest: &Digest,
        decision: AdoptionDecision,
        method: AdoptionMethod,
        evidence: &[AdoptionEvidence],
        confidence: Option<f64>,
        notes: Option<&str>,
        enable: Option<bool>,
    ) -> Result<Value> {
        let mut node = self.node.lock();
        node.handle_unit_request(UnitRequest::RecordAdoption {
            unit_id: unit_id.to_string(),
            revision: revision.to_string(),
            artifact_digest: artifact_digest.clone(),
            decision,
            method,
            evidence: evidence.to_vec(),
            confidence,
            notes: notes.map(str::to_string),
            enable,
        })
    }

    /// Register an observation about one unit version.
    ///
    /// The bytes are stored with their digest and bound to this exact
    /// version, so a later judgement can cite something this installation
    /// really holds. An observation is never empirical acceptance.
    pub fn register_observation(
        &self,
        unit_id: &str,
        revision: &str,
        artifact_digest: &Digest,
        kind: &str,
        bytes: &[u8],
    ) -> Result<Value> {
        let mut node = self.node.lock();
        node.handle_unit_request(UnitRequest::RegisterObservation {
            unit_id: unit_id.to_string(),
            revision: revision.to_string(),
            artifact_digest: artifact_digest.clone(),
            kind: kind.to_string(),
            bytes: bytes.to_vec(),
        })
    }

    /// The observations registered for one unit version.
    pub fn observations(&self, unit_id: &str) -> Result<Vec<CognitionObservation>> {
        let mut node = self.node.lock();
        let value = node.handle_unit_request(UnitRequest::Observations {
            unit_id: unit_id.to_string(),
        })?;
        let entries = value
            .get("observations")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(entries
            .iter()
            .filter_map(|entry| {
               Some(CognitionObservation {
                    digest: entry.get("digest")?.as_str()?.to_string(),
                    kind: entry.get("kind")?.as_str()?.to_string(),
                    bytes: entry.get("bytes").and_then(Value::as_u64).unwrap_or(0),
                    recorded_at: entry
                        .get("recorded_at")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    bytes_held: entry
                        .get("bytes_held")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
        })
        })
            .collect())
    }
    /// The trial evidence this installation recorded for one unit.
    ///
    /// The rows are the node's own durable trial records: a local execution
    /// trial of the artifact, or a comparison trial this installation ran.
    /// They are the facts a judgement can cite.
    pub fn trial_evidence(&self, unit_id: &str, after: &str, limit: u32) -> Result<Value> {
        let mut node = self.node.lock();
        node.handle_unit_request(UnitRequest::Evidence {
            unit_id: Some(unit_id.to_string()),
            after: after.to_string(),
            limit,
        })
    }

    /// Open a durable trial attempt for one unit version.
    ///
    /// The attempt is opened before the trial runs and closed with the
    /// receipts the run produced, so the measurement belongs to the runner
    /// and not to the caller's description of it.
    pub fn begin_trial(
        &self,
        unit_id: &str,
        revision: &str,
        artifact_digest: &Digest,
        kind: &str,
        plan: &Value,
    ) -> Result<Value> {
        let mut node = self.node.lock();
        node.handle_unit_request(UnitRequest::BeginTrial {
            unit_id: unit_id.to_string(),
            revision: revision.to_string(),
            artifact_digest: artifact_digest.clone(),
            kind: kind.to_string(),
            plan: plan.clone(),
        })
    }

    /// Close a trial attempt with the receipts the run produced.
    pub fn finish_trial(
        &self,
        attempt_id: &str,
        outcome: shenko_node::TrialOutcome,
        receipts: &[Value],
    ) -> Result<Value> {
        let mut node = self.node.lock();
        node.handle_unit_request(UnitRequest::FinishTrial {
            attempt_id: attempt_id.to_string(),
            outcome,
            receipts: receipts.to_vec(),
        })
    }

    /// The retained versions of one unit and their supersession relations.
    pub fn lineage(&self, unit_id: &str) -> Result<Value> {
        let mut node = self.node.lock();
        node.handle_unit_request(UnitRequest::Lineage {
            unit_id: unit_id.to_string(),
        })
    }
}

// ------------------------------------------------------------- helpers -----

/// The delegate call that reads one unit's body at one exact version.
pub fn read_op(unit_id: &str, revision: &str) -> String {
    read_op_for(CognitionCall::Delegate, unit_id, revision)
        .expect("a delegating reader can always read a body at its exact version")
}

/// The call that reads one unit's body at one exact version, in the syntax of
/// the reader that makes it.
///
/// A reader whose delegation granted no read has no such call, and inventing
/// the address would only produce a refused call.
pub fn read_op_for(call: CognitionCall, unit_id: &str, revision: &str) -> Option<String> {
    if !call.can_read() {
        return None;
    }
    let verb = match call {
        CognitionCall::Delegate => "delegate",
        CognitionCall::Tool { .. } => "tool",
    };
    Some(format!(
        "[{verb}:{READ_TOOL}:{TOOL_VERSION}] {{\"unit_id\":\"{unit_id}\",\"revision\":\"{revision}\"}}"
    ))
}

/// The delegate call that reads one page of the discovery index.
///
/// The revision is part of the call: a page requested against an older
/// revision is refused instead of mixed with the current installation.
pub fn list_op(start: usize, count: usize, catalog_revision: u64) -> String {
    list_op_for(CognitionCall::Delegate, start, count, catalog_revision)
        .expect("a delegating reader can always page the index")
}

/// The call that reads one page of the discovery index, when this reader has
/// one: a child whose delegation granted neither tool has no paginated index
/// to call, and inventing the address would only produce a refused call.
pub fn list_op_for(
    call: CognitionCall,
    start: usize,
    count: usize,
    catalog_revision: u64,
) -> Option<String> {
    if !call.can_list() {
        return None;
    }
    let verb = match call {
        CognitionCall::Delegate => "delegate",
        CognitionCall::Tool { .. } => "tool",
    };
    Some(format!(
        "[{verb}:{LIST_TOOL}:{TOOL_VERSION}] {{\"start\":{start},\"count\":{count},\"catalog_revision\":{catalog_revision}}}"
    ))
}

fn card_line(card: &CognitionCard, call: Option<CognitionCall>, now: DateTime<Utc>) -> String {
    let mut line = format!(
        "- {}@{} [{}] {}",
        card.unit_id,
        card.revision,
        card.kind.as_str(),
        card.capability_card
    );
    if !card.cues.is_empty() {
        line.push_str(&format!("（何时想到：{}）", card.cues.join("；")));
    }
    if !card.negative_cues.is_empty() {
        line.push_str(&format!("（通常不适用：{}）", card.negative_cues.join("；")));
    }
    if let Some(applicability) = card.applicability.as_ref() {
        line.push_str(&format!("（适用条件：{applicability}）"));
    }
    if card.state != CognitionState::Enabled {
        line.push_str(&format!("（状态：{}）", card.state.as_str()));
    }
    if let Some(perception) = card.perception.as_ref() {
        line.push_str(&format!("（{}）", perception.line(now)));
    }
    match call.and_then(|call| read_op_for(call, &card.unit_id, &card.revision)) {
        Some(op) => line.push_str(&format!("。正文按需读取：{op}")),
        None => line.push_str("。本轮没有读取正文的手段，这里只是已安装单元的名录"),
    }
    line
}

/// The metadata a unit descriptor carries.
pub fn metadata_from_config(config: &Value) -> CognitionMetadata {
    config
        .get("cognition")
        .and_then(|section| serde_json::from_value::<CognitionMetadata>(section.clone()).ok())
        .unwrap_or_default()
}

/// The recorded adoption of a unit version.
pub fn adoption_from_config(config: &Value) -> Option<AdoptionRecord> {
    config
        .pointer("/cognition/adoption")
        .and_then(|value| serde_json::from_value::<AdoptionRecord>(value.clone()).ok())
}

/// The installation revision of one reading of the facts.
fn revision_of(facts: &[shenko_node::InstalledUnitFact]) -> Result<u64> {
    let bytes = serde_json::to_vec(facts)?;
    let digest = Digest::blake3(&bytes);
    // The digest carries its algorithm ("blake3:<64 hex>"): only the
    // canonical hexadecimal part is a number. Parsing the whole string
    // always failed and reported revision zero for every installation.
    let hex = digest
        .canonical_blake3_hex()
        .context("catalog revision digest is not canonical")?;
    let width = hex.len().min(16);
    u64::from_str_radix(&hex[..width], 16).context("catalog revision is not a number")
}

fn unit_from_fact(fact: &shenko_node::InstalledUnitFact) -> CognitionUnit {
    let metadata = metadata_from_config(&fact.descriptor.config);
    let adoption = adoption_from_config(&fact.descriptor.config);
    let state = if !fact.superseded_by.is_empty() {
        CognitionState::Superseded
    } else if adoption
        .as_ref()
        .is_some_and(|adoption| adoption.decision == "rejected")
    {
        CognitionState::Rejected
    } else if adoption
        .as_ref()
        .is_some_and(|adoption| adoption.decision == "retired")
    {
        CognitionState::Retired
    } else if adoption.as_ref().is_some_and(|adoption| {
        adoption.decision == "adopted" && adoption.empirical && adoption.evidence_verified
    }) {
        // Being switched on is not being measured: only a recorded
        // judgement backed by a trial this installation ran reaches this
        // state.
        CognitionState::Adopted
    } else if fact.enabled {
        CognitionState::Enabled
    } else if metadata
        .learning
        .as_ref()
        .is_some_and(|learning| learning.state == "candidate")
    {
        CognitionState::Candidate
    } else {
        CognitionState::Disabled
    };
    CognitionUnit {
        unit_id: fact.unit_id.clone(),
        kind: fact.kind,
        revision: fact.revision.clone(),
        artifact_digest: fact.artifact_digest.0.clone(),
        current: fact.current,
        enabled: fact.enabled,
        state,
        title: metadata.title.clone(),
        capability_card: metadata.capability_card.clone(),
        why: metadata.why.clone(),
        cues: metadata.cues.clone(),
        negative_cues: metadata.negative_cues.clone(),
        applicability: metadata.applicability.clone(),
        knowledge: metadata.knowledge.clone(),
        provenance_refs: metadata.provenance_refs.clone(),
        learning: metadata.learning.clone(),
        adoption,
        supersedes: metadata.supersedes.clone(),
        superseded_by: supersedes_from_fact(fact),
        provides: fact.descriptor.provides.clone(),
        dependencies: dependencies_of(fact),
        imported_at: fact.imported_at.clone(),
        origin_node: fact.origin_node.clone(),
    }
}

fn supersedes_from_fact(fact: &shenko_node::InstalledUnitFact) -> Vec<SupersedesRef> {
    fact.superseded_by
        .iter()
        .map(|relation| SupersedesRef {
            revision: relation.revision.clone(),
            artifact_digest: relation.artifact_digest.0.clone(),
            reason: relation.reason.clone(),
        })
        .collect()
}

fn dependencies_of(fact: &shenko_node::InstalledUnitFact) -> Vec<DependencyFact> {
    fact.edges
        .iter()
        .map(|edge| DependencyFact {
            edge_id: edge.edge_id.clone(),
            strength: match edge.strength {
                shenko_node::DependencyStrength::Required => "required".to_string(),
                shenko_node::DependencyStrength::Recommended => "recommended".to_string(),
            },
            status: match edge.status {
                shenko_node::EdgeStatus::Satisfied => "satisfied".to_string(),
                shenko_node::EdgeStatus::Omitted => "omitted".to_string(),
                shenko_node::EdgeStatus::Unresolved => "unresolved".to_string(),
                shenko_node::EdgeStatus::LegacyAmbiguous => "legacy_ambiguous".to_string(),
            },
            selected_unit_id: edge.selected_unit_id.clone(),
            selected_revision: edge.selected_revision.clone(),
            selected_artifact_digest: edge.selected_artifact_digest.as_ref().map(|digest| digest.0.clone()),
            reason: edge.reason.clone(),
        })
        .collect()
}

pub(crate) fn truncate_utf8(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

fn strip_frontmatter(text: &str) -> String {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return text.trim().to_string();
    }
    let mut body = Vec::new();
    let mut closed = false;
    for line in lines {
        if !closed {
            if line.trim() == "---" {
                closed = true;
            }
            continue;
        }
        body.push(line);
    }
    body.join("\n").trim().to_string()
}

fn parse_supersedes(raw: &str, entry_path: &Path) -> Result<Vec<SupersedesRef>> {
    let raw = raw.trim();
    let inner = raw
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(raw);
    let mut relations = Vec::new();
    for item in inner.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (revision, digest, reason) = match item.split_once('@') {
            Some((revision, digest)) => (revision, digest, None),
            None => anyhow::bail!(
                "{} has a malformed supersedes entry '{item}'; use revision@digest",
                entry_path.display()
            ),
        };
        relations.push(SupersedesRef {
            revision: revision.trim().to_string(),
            artifact_digest: digest.trim().to_string(),
            reason,
        });
    }
    Ok(relations)
}

/// Split `content` into normalized frontmatter keys and the trimmed body.
fn split_frontmatter(
    content: &str,
    entry_path: &Path,
) -> Result<(std::collections::HashMap<String, String>, String)> {
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Ok((std::collections::HashMap::new(), content.trim().to_string()));
    }
    let mut frontmatter = std::collections::HashMap::new();
    let mut body_lines = Vec::new();
    let mut closed = false;
    for line in lines {
        if !closed {
            let trimmed = line.trim();
            if trimmed == "---" {
                closed = true;
                continue;
            }
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some((key, value)) = trimmed.split_once(':') else {
                anyhow::bail!(
                    "{}: invalid frontmatter line (expected `key: value`): {trimmed}",
                    entry_path.display()
                );
            };
            frontmatter.insert(normalize_key(key), unquote(value.trim()));
        } else {
            body_lines.push(line);
        }
    }
    if !closed {
        anyhow::bail!(
            "{}: unterminated frontmatter (missing closing `---`)",
            entry_path.display()
        );
    }
    Ok((frontmatter, body_lines.join("\n").trim().to_string()))
}

fn normalize_key(key: &str) -> String {
    key.trim().to_ascii_lowercase().replace('-', "_")
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

fn list_field(
    frontmatter: &std::collections::HashMap<String, String>,
    keys: &[&str],
) -> Vec<String> {
    let Some(raw) = keys.iter().find_map(|key| frontmatter.get(*key)) else {
        return Vec::new();
    };
    let raw = raw.trim();
    let inner = raw
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(raw);
    inner
        .split(',')
        .map(unquote)
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

fn first_heading(body: &str) -> Option<String> {
    body.lines()
        .find_map(|line| line.trim().strip_prefix('#').map(str::trim))
        .filter(|heading| !heading.is_empty())
        .map(str::to_string)
}

/// The directory a local installation keeps its cognition packages in.
pub fn cognition_package_root(data_dir: &Path) -> PathBuf {
    data_dir.join("cognition")
}

/// Install every authored cognition entry under one directory.
///
/// Every `*.md` file under `package_root` parses as one authored unit entry
/// (frontmatter plus body) and installs into the same installation authority.
/// Entries that fail to parse fail the scan instead of silently skipping, so
/// an unreadable package can never masquerade as an installed unit.
pub fn install_dir(catalog: &CognitionCatalog, package_root: &Path) -> Result<Vec<String>> {
    let mut installed = Vec::new();
    if !package_root.is_dir() {
        return Ok(installed);
    }
    let mut entries: Vec<PathBuf> = Vec::new();
    let mut stack = vec![package_root.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in read.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_some_and(|extension| extension == "md") {
                entries.push(path);
            }
        }
    }
    entries.sort();
    for entry in entries {
        let content = std::fs::read_to_string(&entry)
            .with_context(|| format!("cannot read cognition entry {}", entry.display()))?;
        let package = parse_cognition_entry(&content, &entry)?;
        catalog.install(&package, package_root)?;
        installed.push(format!("{}@{}", package.unit_id, package.revision));
    }
    Ok(installed)
}

impl Service for CognitionCatalog {
    const NAME: &'static str = "shenko.cognition.catalog";
}

/// Publishes the one catalog so any plugin — including a finite child's
/// handler — finds the same installed material by name.
///
/// Publishing does not grant anything: a reader still has to be able to make
/// the call the index names, and the index says so when it cannot.
pub struct CognitionCatalogHost;

impl Plugin for CognitionCatalogHost {
    type Config = Arc<CognitionCatalog>;
    type Input = Arc<CognitionCatalog>;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("cognition-catalog-host")
    }

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        let _publication = ctx.provide(Arc::clone(input)).map_err(std::io::Error::other)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perception::{
        INSTALLATION_UNITS, INSTALLATION_UNITS_SOURCE, INSTALLATION_UNITS_TARGET,
        PerceptionFact, PerceptionObserver, PerceptionPerspective,
    };

    /// A source that answers with one prepared fact.
    struct Fixed(PerceptionFact);

    impl PerceptionObserver for Fixed {
        fn observe(&self) -> Result<PerceptionFact> {
            Ok(self.0.clone())
        }
    }

    fn package(unit_id: &str, kind: UnitKind) -> CognitionPackage {
        CognitionPackage {
            unit_id: unit_id.into(),
            kind,
            revision: "0.1.0".into(),
            title: "fixture".into(),
            capability_card: "fixture card".into(),
            why: Some("a fixture unit for the projection".into()),
            cues: vec![],
            negative_cues: vec![],
            applicability: None,
            knowledge: vec![],
            provenance_refs: vec![],
            learning: None,
            supersedes: vec![],
            provides: vec![],
            dependencies: vec![],
            body: "FIXTURE-BODY-MUST-NOT-BE-DISCLOSED".into(),
            entry_text: "---\nid: fixture\n---\nFIXTURE-BODY-MUST-NOT-BE-DISCLOSED".into(),
            enabled: true,
        }
    }

    /// A real installation authority with one installed unit.
    fn installed(unit_id: &str, kind: UnitKind) -> Arc<CognitionCatalog> {
        let dir = std::env::temp_dir().join(format!("shenko-cognition-{}", uuid::Uuid::new_v4()));
        let root = dir.join("cognition");
        std::fs::create_dir_all(&root).unwrap();
        let node = Arc::new(Mutex::new(shenko_node::Node::open(&dir.join("node")).unwrap()));
        let catalog = Arc::new(CognitionCatalog::new(node));
        catalog.install(&package(unit_id, kind), &root).unwrap();
        catalog
    }

    fn card(catalog: &CognitionCatalog, unit_id: &str) -> CognitionCard {
        catalog
            .cards()
            .unwrap()
            .into_iter()
            .find(|card| card.unit_id == unit_id)
            .unwrap_or_else(|| panic!("unit '{unit_id}' has no card"))
    }

    #[test]
    fn the_shipped_installation_unit_observes_this_nodes_installation() {
        let catalog = installed(INSTALLATION_UNITS, UnitKind::Perception);
        let card = card(&catalog, INSTALLATION_UNITS);
        let Some(PerceptionStatus::Observed { fact }) = card.perception else {
            panic!("the shipped source did not produce a fact")
        };
        assert_eq!(fact.binding.unit_id, INSTALLATION_UNITS);
        assert_eq!(fact.source, INSTALLATION_UNITS_SOURCE);
        assert_eq!(fact.target, INSTALLATION_UNITS_TARGET);
        assert!(fact.summary.contains("认知单元"), "{}", fact.summary);
        // The observation is archived against the exact installed version, so
        // it outlives the definition and can be cited later.
        let archived = catalog.observations(INSTALLATION_UNITS).unwrap();
        assert!(archived.iter().any(|row| row.kind == "perception_observation" && row.bytes_held));
        // Only the card travels; the definition body is read on demand.
        let disclosure = catalog.disclosure().unwrap();
        assert!(disclosure.contains("观察："), "{disclosure}");
        assert!(!disclosure.contains("FIXTURE-BODY-MUST-NOT-BE-DISCLOSED"), "{disclosure}");
    }

    #[test]
    fn a_perception_card_reports_the_fact_or_its_absence() {
        let catalog = installed("test.perception", UnitKind::Perception);
        let unknown = card(&catalog, "test.perception");
        let Some(PerceptionStatus::Unknown { .. }) = unknown.perception else {
            panic!("reading a definition must not fabricate a fact")
        };
        let binding = PerceptionBinding {
            unit_id: "test.perception".into(),
            revision: unknown.revision.clone(),
            artifact_digest: unknown.artifact_digest.clone(),
        };
        catalog
            .perception()
            .bind(
                binding.clone(),
                Arc::new(Fixed(PerceptionFact {
                    binding: binding.clone(),
                    source: "test:probe".into(),
                    target: "connection.fixture".into(),
                    observed_at: "2026-11-05T00:00:00Z".into(),
                    valid_until: None,
                    summary: "连接 X 的 WebSocket 已关闭".into(),
                    evidence_digest: None,
                    perspective: Some(PerceptionPerspective::Own),
                })),
            )
            .unwrap();
        let observed = card(&catalog, "test.perception");
        let Some(PerceptionStatus::Observed { fact }) = observed.perception else {
            panic!("a bound source that answered is an observation")
        };
        assert_eq!(fact.source, "test:probe");
        assert_eq!(fact.target, "connection.fixture");
    }

    #[test]
    fn each_reader_is_told_only_the_calls_it_can_make() {
        let catalog = installed("test.awareness", UnitKind::Awareness);
        let delegate = catalog.render_index(Some(CognitionCall::Delegate)).unwrap();
        assert!(delegate.contains("[delegate:cognition.unit.read:1]"), "{delegate}");
        let tool = catalog.render_index(Some(CognitionCall::Tool { read: true, list: false })).unwrap();
        assert!(tool.contains("[tool:cognition.unit.read:1]"), "{tool}");
        assert!(!tool.contains("[delegate:"), "{tool}");
        let index_only =
            catalog.render_index(Some(CognitionCall::Tool { read: false, list: true })).unwrap();
        assert!(!index_only.contains(READ_TOOL), "{index_only}");
        assert!(index_only.contains("本轮没有读取正文的手段"), "{index_only}");
        // A reader with no granted call still learns what is installed.
        let directory = catalog.render_index(None).unwrap();
        assert!(directory.contains("fixture card"), "{directory}");
        assert!(!directory.contains(READ_TOOL), "{directory}");
        assert!(!directory.contains(LIST_TOOL), "{directory}");
    }
}
