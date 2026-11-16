//! Transfer codec, portable unit descriptors and structural package admission.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use shenko_types::object::Digest;
use super::capabilities::{CapabilityContract, PackageDeclaration, declared_package_capabilities};
use super::resolution::{DependencySelector, DependencySpec, ResolvedDependency};
use super::trials::TrialProvenance;

/// Transfer schema; the node storage schema is versioned independently.
pub const UNIT_SCHEMA: u32 = 3;

/// Bounded limits. Every one of them bounds memory or time; none of them is
/// a product permission decision.
pub const MAX_ARTIFACT_BYTES: usize = 1 << 20;
/// Upper bound of an artifact registered out of line. The bytes are not
/// copied into the node database, so the bound only refuses an absurd
/// declared size; the installer still verifies the real bytes.
pub const MAX_EXTERNAL_ARTIFACT_BYTES: u64 = 1 << 32;
pub const MAX_PACKAGES: usize = 64;
pub const MAX_EDGES: usize = 64;
pub const MAX_PROVIDES: usize = 64;
pub const MAX_TEXT_BYTES: usize = 4096;

// ---------------------------------------------------------------- base64 ---

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode artifact bytes for a transferable package.
pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(B64_ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Decode the artifact bytes of a transferable package.
pub fn b64_decode(text: &str) -> Result<Vec<u8>> {
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    let mut out: Vec<u8> = Vec::with_capacity(text.len() / 4 * 3);
    for ch in text.bytes() {
        if ch == b'\n' || ch == b'\r' || ch == b' ' || ch == b'\t' {
            continue;
        }
        if ch == b'=' {
            break;
        }
        let value = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => bail!("invalid base64 character"),
        } as u32;
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

// ------------------------------------------------------------ unit shape ---

/// The four independently installable unit kinds of docs/specs/cognition-units.md#spec-45-10.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    Perception,
    Awareness,
    Skill,
    Code,
    Metapackage,
    Distribution,
}

impl UnitKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            UnitKind::Perception => "perception",
            UnitKind::Awareness => "awareness",
            UnitKind::Skill => "skill",
            UnitKind::Code => "code",
            UnitKind::Metapackage => "metapackage",
            UnitKind::Distribution => "distribution",
        }
    }

    /// Whether this kind is cognitive *content*.
    ///
    /// Content joins a metapackage only with a separate adoption attitude
    /// (docs/specs/cognition-units.md#spec-51-1/.3); a code plugin is installed, not adopted, and a
    /// sub-metapackage is judged by the attitudes of its own content members.
    pub fn is_content(&self) -> bool {
        matches!(
            self,
            UnitKind::Perception | UnitKind::Awareness | UnitKind::Skill
        )
    }

    /// Whether installing this kind means running a component here.
    pub fn is_executable(&self) -> bool {
        matches!(self, UnitKind::Code)
    }
}

/// Declared platform labels. They are facts about the package, not a verdict
/// about this node: a foreign or missing label stays advisory.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Applicability {
    pub os: Option<String>,
    pub arch: Option<String>,
    pub runtime: Option<String>,
    pub notes: Option<String>,
}

/// The unit identity and metadata that travel with a package.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UnitDescriptor {
    pub unit_id: String,
    pub kind: UnitKind,
    pub revision: String,
    pub artifact_digest: Digest,
    pub provides: Vec<String>,
    pub dependencies: Vec<DependencySpec>,
    pub config: Value,
    pub applicability: Option<Applicability>,
}

impl UnitDescriptor {
    /// The typed capability contracts this version declares providing.
    ///
    /// `provides` keeps its historical string shape so every retained package
    /// still reads. An entry that begins with [`crate::units::CAPABILITY_CONTRACT_PREFIX`] is
    /// a versioned capability contract (`cap.<name>/<major>`, docs/specs/cognition-units.md#spec-51-8) and
    /// is parsed here; every other entry is an opaque topic or capability name
    /// that can satisfy a content dependency only. A malformed contract refuses
    /// the version instead of quietly becoming a topic.
    pub fn contracts(&self) -> Result<Vec<CapabilityContract>> {
        let mut contracts = Vec::new();
        for reference in &self.provides {
            if !CapabilityContract::is_contract_ref(reference) {
                continue;
            }
            let contract = CapabilityContract::parse(reference).with_context(|| {
                format!(
                    "UNIT_CAPABILITY_INVALID: {} revision {} declares {reference:?}",
                    self.unit_id, self.revision
                )
            })?;
            if !contracts.contains(&contract) {
                contracts.push(contract);
            }
        }
        contracts.sort();
        Ok(contracts)
    }

    /// Whether this version declares providing exactly this capability
    /// contract. The major version is part of the identity, so a provider of
    /// `cap.forgetting/2` does not satisfy a `cap.forgetting/1` requirement.
    pub fn provides_contract(&self, contract: &CapabilityContract) -> bool {
        self.provides.iter().any(|reference| {
            CapabilityContract::parse(reference)
                .map(|provided| contract.matches(&provided))
                .unwrap_or(false)
        })
    }

    /// The declared topic or capability names: the `provides` entries that are
    /// not typed capability contracts.
    pub fn provides_topics(&self) -> Vec<&str> {
        self.provides
            .iter()
            .map(String::as_str)
            .filter(|reference| !CapabilityContract::is_contract_ref(reference))
            .collect()
    }

    /// The resource declaration this version authored. Content kinds carry
    /// none, and no version ever declares a conflict with another package.
    pub fn package_declaration(&self) -> Result<PackageDeclaration> {
        declared_package_capabilities(self)
    }
}

/// One unit inside a transferable package: descriptor, actual bytes and the
/// origin node's safe trial provenance. Raw arguments, environment details,
/// paths and captured output never leave the node's local evidence log.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UnitPackage {
    pub unit: UnitDescriptor,
    pub artifact_base64: String,
    pub evidence: Vec<TrialProvenance>,
    /// A v3 export carries each version's own selections. An absent value
    /// denotes an authored/v2 package, whose declarations must be resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_dependencies: Option<Vec<ResolvedDependency>>,
}

/// A transferable closure: the requested unit plus the dependency units this
/// node could actually resolve locally.
///
/// The configuration it carries is the configuration the package declared,
/// copied exactly as authored. This module never reads an environment value,
/// a secret file or a provider grant into a package and exports no such
/// material; a package that declares a secret carries that secret with it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UnitExport {
    pub schema: u32,
    pub root_unit_id: String,
    /// Schema 3 exports bind the root independently of package ordering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_artifact_digest: Option<Digest>,
    pub origin_node: Option<String>,
    /// The enable state of the root unit on the origin node. It travels as
    /// information; the receiving node decides whether to adopt it.
    pub enabled: bool,
    pub packages: Vec<UnitPackage>,
}

/// An artifact whose bytes live outside the node's inline object store.
///
/// `store` names the object store that holds the bytes (`cas` for the
/// Shenko content-addressed store) and `locator` is the installation-local
/// address of the object. The digest stays the portable identity: a peer
/// receives the digest and re-registers the unit against its own bytes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExternalArtifact {
    pub digest: Digest,
    pub bytes: u64,
    pub store: String,
    pub locator: String,
}

/// Where the bytes of one artifact actually live.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum UnitArtifactSource {
    /// The bytes are in this installation's inline object store.
    Inline,
    /// The bytes live outside the inline store and are addressed by the
    /// installation-local locator.
    External {
        store: String,
        locator: String,
        bytes: u64,
    },
}

/// The out-of-line artifact a unit version declares, when it declares one.
pub(super) fn external_artifact_of(descriptor: &UnitDescriptor) -> Option<ExternalArtifact> {
    let value = descriptor.config.get("external_artifact")?;
    let digest = Digest(value.get("digest")?.as_str()?.to_string());
    let bytes = value.get("bytes")?.as_u64()?;
    let store = value.get("store")?.as_str()?.to_string();
    let locator = value.get("locator")?.as_str()?.to_string();
    Some(ExternalArtifact {
        digest,
        bytes,
        store,
        locator,
    })
}

pub(super) fn validate_descriptor(unit: &UnitDescriptor) -> Result<()> {
    ensure!(
        !unit.unit_id.is_empty() && unit.unit_id.len() <= 128,
        "invalid unit identity"
    );
    ensure!(
        !unit.revision.is_empty() && unit.revision.len() <= 64,
        "invalid unit revision"
    );
    ensure!(
        unit.dependencies.len() <= MAX_EDGES,
        "too many declared dependencies"
    );
    ensure!(
        unit.provides.len() <= MAX_PROVIDES,
        "too many declared capabilities"
    );
    let mut edge_ids: Vec<&str> = Vec::new();
    for edge in &unit.dependencies {
        ensure!(
            !edge.edge_id.is_empty() && edge.edge_id.len() <= 128,
            "invalid dependency edge identity"
        );
        ensure!(
            !edge_ids.contains(&edge.edge_id.as_str()),
            "duplicate dependency edge identity"
        );
        edge_ids.push(&edge.edge_id);
        match &edge.selector {
            DependencySelector::Exact { unit_id, revision_requirement, artifact_digest, contract_requirement: _ } => {
                ensure!(
                    !unit_id.is_empty() && unit_id.len() <= 128,
                    "invalid exact dependency target"
                );
                ensure!(
                    revision_requirement.is_some() || artifact_digest.is_some(),
                    "an exact dependency must pin a revision requirement or an artifact digest"
                );
            }
            DependencySelector::Abstract {
                capability_ref,
                topic_ref,
            } => {
                ensure!(
                    capability_ref.is_some() || topic_ref.is_some(),
                    "an abstract dependency must name a capability or a topic"
                );
            }
        }
    }
    Ok(())
}
