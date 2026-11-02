//! Installable cognition units — SPEC §45.1–45.10.
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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_node::{
    AdoptionDecision, AdoptionEvidence, AdoptionMethod, Applicability, DependencySpec, Node,
    UnitDescriptor, UnitExport, UnitKind, UnitPackage, UnitRequest, UNIT_SCHEMA,
};
use shenko_types::object::Digest;

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
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
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
    /// Whether the evidence carries a measurement. Only a trial attempt can.
    pub empirical: bool,
    pub notes: Option<String>,
    pub recorded_at: String,
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
/// The authored form is the minimal package of SPEC §45.2: an entry file with
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
/// The frontmatter is the minimal form of SPEC §45.2: `key: value` or
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
    ];
    SEEDS
        .iter()
        .map(|(path, content)| parse_cognition_entry(content, Path::new(path)))
        .collect()
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
    /// The exact call that reads this unit's body at this version.
    pub read_op: String,
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
}

impl CognitionCatalog {
    /// Wrap the installation authority this instance already owns.
    pub fn new(node: Arc<Mutex<Node>>) -> Self {
        Self {
            node,
            generation: RwLock::new(0),
        }
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
        let facts = self.node.lock().installed_unit_facts()?;
        let bytes = serde_json::to_vec(&facts)?;
        let digest = Digest::blake3(&bytes);
        let hex = digest.0.as_str();
        let width = hex.len().min(16);
        Ok(u64::from_str_radix(&hex[..width], 16).unwrap_or(0))
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
        let node = self.node.lock();
        let facts = node.installed_unit_facts()?;
        Ok(facts.iter().map(unit_from_fact).collect())
    }

    /// Every retained version of one unit, newest relation first.
    pub fn versions(&self, unit_id: &str) -> Result<Vec<CognitionUnit>> {
        let node = self.node.lock();
        let facts = node.retained_unit_facts(unit_id)?;
        Ok(facts.iter().map(unit_from_fact).collect())
    }

    /// The short index of the installed cognition units.
    ///
    /// The index carries the card of every unit that authored one. A unit
    /// without a card is still installed and still readable; it simply has
    /// nothing to disclose before it is read.
    pub fn cards(&self) -> Result<Vec<CognitionCard>> {
        Ok(self
            .units()?
            .into_iter()
            .filter(|unit| {
                !unit.capability_card.is_empty()
                    && matches!(
                        unit.state,
                        CognitionState::Enabled
                            | CognitionState::Candidate
                            | CognitionState::Adopted
                    )
            })
            .map(|unit| CognitionCard {
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
                read_op: read_op(&unit.unit_id, &unit.revision),
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
        let revision = self.catalog_revision()?;
        let cards = self.cards()?;
        let total = cards.len();
        // The footer names the total, the revision and the exact next call.
        // Its size is bounded by construction, so reserving room for it keeps
        // the whole disclosure inside the bound.
        const FOOTER_RESERVE: usize = 320;
        let budget = MAX_INDEX_BYTES.saturating_sub(FOOTER_RESERVE);
        let mut lines: Vec<String> = Vec::new();
        let mut used = 0_usize;
        for card in &cards {
            let line = card_line(card);
            if used + line.len() + 1 > budget {
                break;
            }
            used += line.len();
            lines.push(line);
        }
        let mut shown = lines.len();
        let footer = match (shown < total).then_some(shown) {
            Some(next_start) => format!(
                "（共 {total} 个单元，此处显示 {shown} 个；目录修订 {revision}；其余按需分页读取：{}）\n",
                list_op(next_start, MAX_PAGE_CARDS, revision)
            ),
            None => format!("（共 {total} 个单元，已全部显示；目录修订 {revision}）\n"),
        };
        let mut text = String::new();
        for line in &lines {
            text.push_str(line);
            text.push('\n');
        }
        text.push_str(&footer);
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
                text.push_str(&match (shown < total).then_some(shown) {
                    Some(next_start) => format!(
                        "（共 {total} 个单元，此处显示 {shown} 个；目录修订 {revision}；其余按需分页读取：{}）\n",
                        list_op(next_start, MAX_PAGE_CARDS, revision)
                    ),
                    None => format!("（共 {total} 个单元，已全部显示；目录修订 {revision}）\n"),
                });
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
        let cards = self.cards()?;
        let total = cards.len();
        let start = start.min(total);
        let end = start.saturating_add(count).min(total);
        let page = cards[start..end].to_vec();
        let next_start = (end < total).then_some(end);
        Ok(CognitionDiscovery {
            catalog_revision: self.catalog_revision()?,
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
    /// empirical acceptance.
    pub fn record_adoption(
        &self,
        unit_id: &str,
        revision: &str,
        artifact_digest: &Digest,
        decision: AdoptionDecision,
        method: AdoptionMethod,
        evidence: &[AdoptionEvidence],
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
    format!(
        "[delegate:cognition.unit.read:1] {{\"unit_id\":\"{unit_id}\",\"revision\":\"{revision}\"}}"
    )
}

/// The delegate call that reads one page of the discovery index.
///
/// The revision is part of the call: a page requested against an older
/// revision is refused instead of mixed with the current installation.
pub fn list_op(start: usize, count: usize, catalog_revision: u64) -> String {
    format!(
        "[delegate:cognition.unit.list:1] {{\"start\":{start},\"count\":{count},\"catalog_revision\":{catalog_revision}}}"
    )
}

fn card_line(card: &CognitionCard) -> String {
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
    line.push_str(&format!("。正文按需读取：{}", card.read_op));
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

fn truncate_utf8(text: &str, limit: usize) -> (String, bool) {
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
