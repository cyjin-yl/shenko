//! Metapackage membership, distribution declarations and capability-provider slots.

use anyhow::{Context, Result, ensure};
use crate::Node;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_types::object::Digest;
use std::collections::BTreeMap;
use super::adoption::{ContentAdoption, MacroContentAdoption, UnitAdoptionProjection};
use super::capabilities::{
    AtomicCapability, CAPABILITY_CONTRACT_PREFIX, CapabilityContract, MAX_DECLARED_HOST,
    PackageCapabilities, atomic_conflict, declared_package_capabilities,
};
use super::codec::{MAX_TEXT_BYTES, UnitDescriptor, UnitKind};
use super::resolution::{ReadinessState, revision_matches};

/// Bounds on one metapackage or distribution declaration. They bound the
/// admission walk; none of them is a product permission decision.
pub const MAX_MACRO_MEMBERS: usize = 64;
/// Bound on nested metapackages while a macro closure is walked.
pub const MAX_MACRO_DEPTH: usize = 4;

/// How one metapackage member's admission is judged.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemberAdoption {
    /// The member's own kind decides: content needs an authenticated adoption
    /// attitude, code needs installation only. This is the default and it can
    /// never be used to waive the content requirement.
    #[default]
    Required,
    /// The author additionally requires an adoption attitude for a code or
    /// metapackage member.
    Attested,
}

/// One declared member of a metapackage.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MacroMemberSpec {
    /// The member's unit id. A unit id, never a capability contract.
    pub member_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_requirement: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_digest: Option<Digest>,
    /// A named slot: the member must itself declare providing this exact
    /// capability contract, so a consumer resolves a role — a revision
    /// projection rule, an awareness host — without naming a package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<CapabilityContract>,
    #[serde(default)]
    pub adoption: MemberAdoption,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

/// The member list one metapackage version authors.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MetapackageBody {
    #[serde(default)]
    pub members: Vec<MacroMemberSpec>,
}

impl MetapackageBody {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.members.len() <= MAX_MACRO_MEMBERS,
            "UNIT_METAPACKAGE_INVALID: at most {MAX_MACRO_MEMBERS} members"
        );
        let mut ids: Vec<&str> = Vec::new();
        for member in &self.members {
            ensure!(
                !member.member_id.is_empty() && member.member_id.len() <= 128,
                "UNIT_METAPACKAGE_INVALID: invalid member identity {:?}",
                member.member_id
            );
            ensure!(
                !member.member_id.starts_with(CAPABILITY_CONTRACT_PREFIX),
                "UNIT_METAPACKAGE_INVALID: member {:?} uses the reserved capability namespace",
                member.member_id
            );
            ensure!(
                !ids.contains(&member.member_id.as_str()),
                "UNIT_METAPACKAGE_INVALID: duplicate member identity {:?}",
                member.member_id
            );
            ids.push(&member.member_id);
            if let Some(requirement) = &member.revision_requirement {
                ensure!(
                    !requirement.trim().is_empty(),
                    "UNIT_METAPACKAGE_INVALID: member {:?} declares an empty revision requirement",
                    member.member_id
                );
            }
            if let Some(purpose) = &member.purpose {
                ensure!(
                    purpose.len() <= MAX_TEXT_BYTES,
                    "UNIT_METAPACKAGE_INVALID: member {:?} purpose exceeds the text bound",
                    member.member_id
                );
            }
        }
        Ok(())
    }

    pub fn member(&self, member_id: &str) -> Option<&MacroMemberSpec> {
        self.members.iter().find(|member| member.member_id == member_id)
    }
}

/// The member list a version declares, or `None` for a version that declares
/// none. A metapackage without a member declaration, or a non-metapackage that
/// declares members, is refused: a macro that silently installs nothing is not
/// a macro.
pub fn declared_metapackage(descriptor: &UnitDescriptor) -> Result<Option<MetapackageBody>> {
    let Some(value) = descriptor.config.get("metapackage") else {
        ensure!(
            descriptor.kind != UnitKind::Metapackage,
            "UNIT_METAPACKAGE_INVALID: {} revision {} is a metapackage without a member declaration",
            descriptor.unit_id,
            descriptor.revision
        );
        return Ok(None);
    };
    ensure!(
        descriptor.kind == UnitKind::Metapackage,
        "UNIT_METAPACKAGE_INVALID: {} revision {} declares members but its kind is {}",
        descriptor.unit_id,
        descriptor.revision,
        descriptor.kind.as_str()
    );
    let body: MetapackageBody = serde_json::from_value(value.clone()).with_context(|| {
        format!(
            "UNIT_METAPACKAGE_INVALID: {} revision {} declares unreadable members",
            descriptor.unit_id, descriptor.revision
        )
    })?;
    body.validate()?;
    Ok(Some(body))
}

/// What the installation's distribution declares about one named target's
/// expected access.
///
/// It is installable content (docs/specs/cognition-units.md#spec-51-3), not a kernel constant: the kernel
/// never decides which access a deployment expects for a target. `expected`
/// uses the same typed requirement shape a package declares for itself, so an
/// expectation is compared with a real grant instead of matched as a keyword.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TargetAccessExpectation {
    pub target_id: String,
    pub expected: PackageCapabilities,
    /// Requirements this runtime's mechanism does not provide at all (no
    /// process spawn, no raw sockets on this build). They are facts about the
    /// mechanism that the expectation was written against, not a product
    /// permission decision.
    #[serde(default)]
    pub unavailable_requirements: Vec<String>,
}

/// One metapackage a distribution preinstalls.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PreinstalledMacro {
    pub unit_id: String,
    /// Parallel macros are peers; none contains another (docs/specs/cognition-units.md#spec-51-3).
    #[serde(default)]
    pub parallel: bool,
    /// The distribution is designed to depend on this macro. It records a
    /// closure fact; it is not a boot identity gate.
    #[serde(default)]
    pub design_dependency: bool,
}

/// The unit a distribution uses by default for one capability contract. It is
/// replaceable data: another unit may provide the same contract instead.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CapabilityProviderDefault {
    pub contract: CapabilityContract,
    pub unit_id: String,
}

/// The distribution data one distribution version authors.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DistributionBody {
    #[serde(default)]
    pub targets: Vec<TargetAccessExpectation>,
    #[serde(default)]
    pub macros: Vec<PreinstalledMacro>,
    #[serde(default)]
    pub capability_providers: Vec<CapabilityProviderDefault>,
}

impl DistributionBody {
    pub fn validate(&self) -> Result<()> {
        let mut targets: Vec<&str> = Vec::new();
        for target in &self.targets {
            ensure!(
                !target.target_id.is_empty() && target.target_id.len() <= 128,
                "UNIT_DISTRIBUTION_INVALID: invalid target identity {:?}",
                target.target_id
            );
            ensure!(
                !targets.contains(&target.target_id.as_str()),
                "UNIT_DISTRIBUTION_INVALID: target {:?} is declared twice",
                target.target_id
            );
            targets.push(&target.target_id);
            target.expected.validate()?;
            ensure!(
                target.unavailable_requirements.len() <= MAX_DECLARED_HOST,
                "UNIT_DISTRIBUTION_INVALID: target {:?} declares too many unavailable requirements",
                target.target_id
            );
        }
        let mut macros: Vec<&str> = Vec::new();
        for name in &self.macros {
            ensure!(
                !name.unit_id.is_empty() && name.unit_id.len() <= 128,
                "UNIT_DISTRIBUTION_INVALID: invalid preinstalled metapackage identity {:?}",
                name.unit_id
            );
            ensure!(
                !name.unit_id.starts_with(CAPABILITY_CONTRACT_PREFIX),
                "UNIT_DISTRIBUTION_INVALID: preinstalled metapackage {:?} uses the reserved capability namespace",
                name.unit_id
            );
            ensure!(
                !macros.contains(&name.unit_id.as_str()),
                "UNIT_DISTRIBUTION_INVALID: metapackage {:?} is preinstalled twice",
                name.unit_id
            );
            macros.push(&name.unit_id);
        }
        let mut contracts: Vec<&CapabilityContract> = Vec::new();
        for provider in &self.capability_providers {
            ensure!(
                !provider.unit_id.is_empty() && provider.unit_id.len() <= 128,
                "UNIT_DISTRIBUTION_INVALID: invalid provider default identity {:?}",
                provider.unit_id
            );
            ensure!(
                !contracts.iter().any(|contract| **contract == provider.contract),
                "UNIT_DISTRIBUTION_INVALID: capability contract {} has two default providers",
                provider.contract
            );
            contracts.push(&provider.contract);
        }
        Ok(())
    }

    pub fn provider_for(&self, contract: &CapabilityContract) -> Option<&str> {
        self.capability_providers
            .iter()
            .find(|provider| provider.contract.matches(contract))
            .map(|provider| provider.unit_id.as_str())
    }
}

pub fn declared_distribution(descriptor: &UnitDescriptor) -> Result<Option<DistributionBody>> {
    let Some(value) = descriptor.config.get("distribution") else {
        ensure!(
            descriptor.kind != UnitKind::Distribution,
            "UNIT_DISTRIBUTION_INVALID: {} revision {} is a distribution without a declaration",
            descriptor.unit_id,
            descriptor.revision
        );
        return Ok(None);
    };
    ensure!(
        descriptor.kind == UnitKind::Distribution,
        "UNIT_DISTRIBUTION_INVALID: {} revision {} declares distribution data but its kind is {}",
        descriptor.unit_id,
        descriptor.revision,
        descriptor.kind.as_str()
    );
    let body: DistributionBody = serde_json::from_value(value.clone()).with_context(|| {
        format!(
            "UNIT_DISTRIBUTION_INVALID: {} revision {} declares unreadable distribution data",
            descriptor.unit_id, descriptor.revision
        )
    })?;
    body.validate()?;
    Ok(Some(body))
}

/// Which unit version a distribution declaration came from. A declaration is
/// read with its provenance, so a projection can name the content that carried
/// it instead of presenting it as a kernel table.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeclaredBy {
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: Digest,
}

/// One target expectation with the content that declared it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeclaredTargetAccess {
    pub expectation: TargetAccessExpectation,
    pub declared_by: DeclaredBy,
}

/// Where one metapackage member stands on this node.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MembershipStatus {
    /// A content member with an authenticated adoption attitude: it joined.
    Joined,
    /// A code member that is installed here.
    CodeInstalled,
    /// The member version is installed but its adoption attitude is missing,
    /// undetermined locally, or refuted locally. It has not joined.
    Unadopted,
    /// No installed version matches the member declaration.
    Absent,
    /// A version matched the identity but not the declaration (a named slot
    /// whose contract it does not provide, or a retained version that no longer
    /// satisfies the requirement), or its own closure is unresolved.
    Unresolved,
}

impl MembershipStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            MembershipStatus::Joined => "joined",
            MembershipStatus::CodeInstalled => "code_installed",
            MembershipStatus::Unadopted => "unadopted",
            MembershipStatus::Absent => "absent",
            MembershipStatus::Unresolved => "unresolved",
        }
    }

    /// Whether the member is part of the macro on this node.
    pub fn joined(&self) -> bool {
        matches!(self, MembershipStatus::Joined | MembershipStatus::CodeInstalled)
    }
}

/// One metapackage member's standing on this node.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MembershipResolution {
    pub macro_id: String,
    /// The metapackages that reached this member, outermost first. Empty for a
    /// direct member of `macro_id`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<String>,
    pub member_id: String,
    /// The named slot this member fills, when the declaration names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<CapabilityContract>,
    pub status: MembershipStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_kind: Option<UnitKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_artifact_digest: Option<Digest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adoption: Option<ContentAdoption>,
    /// Local facts. An installed member that is not enabled here has not joined
    /// a running macro, and a member installed on a peer never joins locally.
    #[serde(default)]
    pub enabled: bool,
    pub readiness: ReadinessState,
    pub reason: String,
}

/// One provider of a metapackage slot, as installed on this node.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemberSelection {
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: Digest,
    pub member_kind: UnitKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adoption: Option<ContentAdoption>,
    pub enabled: bool,
    pub readiness: ReadinessState,
}

/// The providers a named metapackage slot has on this node.
///
/// `selected` is an index into `providers`, never a kernel default: when no
/// provider is enabled and admitted the slot is omitted here, and a consumer
/// reports that instead of claiming a rule was applied.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MacroSlot {
    pub macro_id: String,
    pub contract: CapabilityContract,
    #[serde(default)]
    pub providers: Vec<MemberSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<usize>,
    /// True when more than one admitted provider is enabled and their actually
    /// used atomic capabilities do not conflict.
    #[serde(default)]
    pub coexisting: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict: Option<String>,
}

impl Node {
    // --------------------------------------------- metapackage/distribution queries -----

    /// Walk a metapackage's declared members, resolve each against installed
    /// units, and check adoption via the installation's own adoption records.
    ///
    /// A member that is content (perception, awareness, skill) **must** carry
    /// an authenticated adoption attitude whose evidence verifies locally
    /// (docs/specs/cognition-units.md#spec-51-1, docs/specs/cognition-units.md#spec-51-3, docs/specs/cognition-units.md#spec-51-8). Code members need installation only.
    /// A member installed on a peer never joins locally: declarations crossing
    /// nodes never imply local readiness.
    pub(super) fn query_metapackage(&self, unit_id: &str) -> Result<Value> {
        let unit = self
            .installed_unit(unit_id)?
            .with_context(|| format!("UNIT_NOT_FOUND: {unit_id} is not installed on this node"))?;
        let body = declared_metapackage(&unit.descriptor)?
            .with_context(|| format!("UNIT_NOT_METAPACKAGE: {unit_id} does not declare a metapackage body"))?;

        let adoption_projection = UnitAdoptionProjection::new(&self.conn);
        let mut memberships: Vec<MembershipResolution> = Vec::new();
        let mut slots: Vec<MacroSlot> = Vec::new();
        let mut contract_providers: BTreeMap<String, Vec<MemberSelection>> = BTreeMap::new();

        for spec in &body.members {
            let resolution = self.resolve_macro_member(
                unit_id,
                spec,
                &adoption_projection,
            )?;
            // Track providers for named contract slots.
            if let Some(contract) = &spec.contract {
                if let (Some(revision), Some(digest)) = (
                    resolution.selected_revision.as_ref(),
                    resolution.selected_artifact_digest.as_ref(),
                ) {
                    let selection = MemberSelection {
                        unit_id: spec.member_id.clone(),
                        revision: revision.clone(),
                        artifact_digest: digest.clone(),
                        member_kind: resolution.member_kind.unwrap_or(UnitKind::Code),
                        adoption: resolution.adoption.clone(),
                        enabled: resolution.enabled,
                        readiness: resolution.readiness,
                    };
                    contract_providers
                        .entry(contract.text())
                        .or_default()
                        .push(selection);
                }
            }
            memberships.push(resolution);
        }

        // Build named slots from the collected providers.
        for (contract_text, providers) in &contract_providers {
            let contract = CapabilityContract::parse(contract_text)?;
            let slot = self.build_macro_slot(unit_id, &contract, providers)?;
            slots.push(slot);
        }

        Ok(json!({
            "unit_id": unit_id,
            "kind": "metapackage",
            "revision": unit.revision,
            "artifact_digest": unit.artifact_digest,
            "members": memberships,
            "slots": slots,
            "member_count": body.members.len(),
        }))
    }

    /// Resolve one metapackage member spec against installed/retained units.
    fn resolve_macro_member(
        &self,
        macro_id: &str,
        spec: &MacroMemberSpec,
        adoption: &dyn MacroContentAdoption,
    ) -> Result<MembershipResolution> {
        // Look up the member as an installed unit.
        let installed = self.installed_unit(&spec.member_id)?;
        let Some(unit) = installed else {
            return Ok(MembershipResolution {
                macro_id: macro_id.to_string(),
                path: vec![],
                member_id: spec.member_id.clone(),
                contract: spec.contract.clone(),
                status: MembershipStatus::Absent,
                member_kind: None,
                selected_revision: None,
                selected_artifact_digest: None,
                adoption: None,
                enabled: false,
                readiness: ReadinessState::NotInstalled,
                reason: format!("{} is not installed on this node", spec.member_id),
            });
        };

        // Check revision requirement if the spec pins one.
        if let Some(requirement) = &spec.revision_requirement {
            if !revision_matches(requirement, &unit.revision) {
                return Ok(MembershipResolution {
                    macro_id: macro_id.to_string(),
                    path: vec![],
                    member_id: spec.member_id.clone(),
                    contract: spec.contract.clone(),
                    status: MembershipStatus::Unresolved,
                    member_kind: Some(unit.descriptor.kind),
                    selected_revision: Some(unit.revision.clone()),
                    selected_artifact_digest: Some(unit.artifact_digest.clone()),
                    adoption: None,
                    enabled: unit.enabled,
                    readiness: ReadinessState::Unresolved,
                    reason: format!(
                        "installed revision {} does not satisfy requirement {requirement}",
                        unit.revision
                    ),
                });
            }
        }

        // Check artifact digest if pinned.
        if let Some(expected) = &spec.artifact_digest {
            if &unit.artifact_digest != expected {
                return Ok(MembershipResolution {
                    macro_id: macro_id.to_string(),
                    path: vec![],
                    member_id: spec.member_id.clone(),
                    contract: spec.contract.clone(),
                    status: MembershipStatus::Unresolved,
                    member_kind: Some(unit.descriptor.kind),
                    selected_revision: Some(unit.revision.clone()),
                    selected_artifact_digest: Some(unit.artifact_digest.clone()),
                    adoption: None,
                    enabled: unit.enabled,
                    readiness: ReadinessState::Unresolved,
                    reason: format!(
                        "installed artifact digest does not match the pinned value"
                    ),
                });
            }
        }

        // Check contract: if the spec names one, the member must declare
        // providing it.
        if let Some(contract) = &spec.contract {
            if !unit.descriptor.provides_contract(contract) {
                return Ok(MembershipResolution {
                    macro_id: macro_id.to_string(),
                    path: vec![],
                    member_id: spec.member_id.clone(),
                    contract: Some(contract.clone()),
                    status: MembershipStatus::Unresolved,
                    member_kind: Some(unit.descriptor.kind),
                    selected_revision: Some(unit.revision.clone()),
                    selected_artifact_digest: Some(unit.artifact_digest.clone()),
                    adoption: None,
                    enabled: unit.enabled,
                    readiness: ReadinessState::Unresolved,
                    reason: format!(
                        "{} does not declare providing {}",
                        spec.member_id,
                        contract
                    ),
                });
            }
        }

        // Adoption check: content kinds require authenticated adoption.
        let needs_adoption = unit.descriptor.kind.is_content()
            || spec.adoption == MemberAdoption::Attested;
        let adoption_result = if needs_adoption {
            adoption.adoption(
                &spec.member_id,
                &unit.revision,
                &unit.artifact_digest,
            )?
        } else {
            None
        };

        let (status, reason) = if needs_adoption {
            match &adoption_result {
                None => (
                    MembershipStatus::Unadopted,
                    format!(
                        "{} is content or attested; no adoption attitude found",
                        spec.member_id
                    ),
                ),
                Some(ca) if !ca.accepts(&unit.artifact_digest) => {
                    let refusal = ca
                        .refusal(&unit.artifact_digest)
                        .unwrap_or_else(|| "adoption does not accept this revision".into());
                    (MembershipStatus::Unadopted, refusal)
                }
                Some(_) => {
                    let (readiness, reason) = self.readiness_of(&unit)?;
                    if readiness == ReadinessState::Ready {
                        (
                            MembershipStatus::Joined,
                            format!("{} is adopted and ready", spec.member_id),
                        )
                    } else {
                        (
                            MembershipStatus::CodeInstalled,
                            format!("{} is adopted but readiness is {:?}: {reason}", spec.member_id, readiness),
                        )
                    }
                }
            }
        } else {
            // Code member: installation is sufficient, no adoption needed.
            let (readiness, reason) = self.readiness_of(&unit)?;
            if readiness == ReadinessState::Ready {
                (
                    MembershipStatus::Joined,
                    format!("{} is installed and ready", spec.member_id),
                )
            } else {
                (
                    MembershipStatus::CodeInstalled,
                    format!("{} is installed but readiness is {:?}: {reason}", spec.member_id, readiness),
                )
            }
        };

        let (readiness, _) = self.readiness_of(&unit)?;

        Ok(MembershipResolution {
            macro_id: macro_id.to_string(),
            path: vec![],
            member_id: spec.member_id.clone(),
            contract: spec.contract.clone(),
            status,
            member_kind: Some(unit.descriptor.kind),
            selected_revision: Some(unit.revision.clone()),
            selected_artifact_digest: Some(unit.artifact_digest.clone()),
            adoption: adoption_result,
            enabled: unit.enabled,
            readiness,
            reason,
        })
    }

    /// Build a named contract slot from the collected providers.
    ///
    /// Multiple admitted providers coexist when their actually used atomic
    /// capabilities do not conflict (docs/specs/cognition-units.md#spec-51-8). Selection: the first
    /// admitted and enabled provider is the selected one; a slot where no
    /// provider is admitted stays unselected, and a consumer reports that
    /// omission rather than claiming a rule was applied.
    fn build_macro_slot(
        &self,
        macro_id: &str,
        contract: &CapabilityContract,
        providers: &[MemberSelection],
    ) -> Result<MacroSlot> {
        let admitted: Vec<&MemberSelection> = providers
            .iter()
            .filter(|provider| {
                provider
                    .adoption
                    .as_ref()
                    .is_none_or(|ca| ca.accepts(&provider.artifact_digest))
            })
            .collect();

        // Check coexistence: admitted providers whose used atomic capabilities
        // conflict cannot coexist.
        let mut coexisting = admitted.len() > 1;
        let mut conflict: Option<String> = None;
        if admitted.len() > 1 {
            for (i, a) in admitted.iter().enumerate() {
                for b in admitted.iter().skip(i + 1) {
                    let a_atoms = self.provider_atoms(&a.unit_id, &a.revision, &a.artifact_digest)?;
                    let b_atoms = self.provider_atoms(&b.unit_id, &b.revision, &b.artifact_digest)?;
                    if let Some(reason) = atomic_conflict(&a_atoms, &b_atoms) {
                        coexisting = false;
                        conflict = Some(format!(
                            "{} and {} conflict: {reason}",
                            a.unit_id, b.unit_id
                        ));
                        break;
                    }
                }
                if conflict.is_some() {
                    break;
                }
            }
        }

        // Select: first admitted + enabled provider.
        let selected = providers.iter().position(|provider| {
            provider.enabled
                && provider
                    .adoption
                    .as_ref()
                    .is_none_or(|ca| ca.accepts(&provider.artifact_digest))
        });

        Ok(MacroSlot {
            macro_id: macro_id.to_string(),
            contract: contract.clone(),
            providers: providers.to_vec(),
            selected,
            coexisting,
            conflict,
        })
    }

    /// Read the atomic capabilities a version declares.
    fn provider_atoms(
        &self,
        unit_id: &str,
        revision: &str,
        digest: &Digest,
    ) -> Result<Vec<AtomicCapability>> {
        let fact = self.retained_unit_fact(unit_id, revision, digest)?;
        match fact {
            Some(f) => {
                let decl = declared_package_capabilities(&f.descriptor)?;
                Ok(decl
                    .capabilities()
                    .map(|c| c.normalized().atoms)
                    .unwrap_or_default())
            }
            None => Ok(vec![]),
        }
    }

    /// Walk a distribution's declarations and return the complete projection.
    ///
    /// Target-access expectations replace the kernel's `NAMED_TARGETS` const
    /// table (docs/specs/cognition-units.md#spec-51-3). Preinstalled macros name the metapackages the
    /// distribution ships by design. Capability-provider defaults name the
    /// replaceable provider selections. All are declarations — facts about the
    /// distribution version, never grants of local readiness.
    pub(super) fn query_distribution(&self, unit_id: &str) -> Result<Value> {
        let unit = self
            .installed_unit(unit_id)?
            .with_context(|| format!("UNIT_NOT_FOUND: {unit_id} is not installed on this node"))?;
        let body = declared_distribution(&unit.descriptor)?
            .with_context(|| format!("UNIT_NOT_DISTRIBUTION: {unit_id} does not declare a distribution body"))?;

        let declared_by = DeclaredBy {
            unit_id: unit.unit_id.clone(),
            revision: unit.revision.clone(),
            artifact_digest: unit.artifact_digest.clone(),
        };

        // Each target-access expectation carries its provenance.
        let mut targets: Vec<Value> = Vec::new();
        for expectation in &body.targets {
            let access = DeclaredTargetAccess {
                expectation: expectation.clone(),
                declared_by: declared_by.clone(),
            };
            targets.push(serde_json::to_value(&access)?);
        }

        // Resolve preinstalled macros.
        let mut macros: Vec<Value> = Vec::new();
        for macro_spec in &body.macros {
            let installed = self.installed_unit(&macro_spec.unit_id)?.is_some();
            let is_metapackage = self
                .installed_unit(&macro_spec.unit_id)?
                .is_some_and(|u| u.descriptor.kind == UnitKind::Metapackage);
            macros.push(json!({
                "macro_id": macro_spec.unit_id,
                "parallel": macro_spec.parallel,
                "design_dependency": macro_spec.design_dependency,
                "installed": installed,
                "is_metapackage": is_metapackage,
            }));
        }

        // Resolve capability-provider defaults.
        let mut capability_defaults: Vec<Value> = Vec::new();
        for default in &body.capability_providers {
            let provider_installed = self.installed_unit(&default.unit_id)?.is_some();
            let provides = self
                .installed_unit(&default.unit_id)?
                .is_some_and(|u| u.descriptor.provides_contract(&default.contract));
            capability_defaults.push(json!({
                "contract": default.contract.text(),
                "provider_unit_id": default.unit_id,
                "installed": provider_installed,
                "declares_providing": provides,
            }));
        }

        Ok(json!({
            "unit_id": unit_id,
            "kind": "distribution",
            "revision": unit.revision,
            "artifact_digest": unit.artifact_digest,
            "targets": targets,
            "macros": macros,
            "capability_providers": capability_defaults,
        }))
    }

    /// Resolve one named capability-contract slot across installed providers.
    pub(super) fn query_slot(&self, macro_id: &str, contract_text: &str) -> Result<Value> {
        let contract = CapabilityContract::parse(contract_text)?;
        let unit = self
            .installed_unit(macro_id)?
            .with_context(|| format!("UNIT_NOT_FOUND: {macro_id} is not installed on this node"))?;
        let body = declared_metapackage(&unit.descriptor)?
            .with_context(|| format!("UNIT_NOT_METAPACKAGE: {macro_id} does not declare a metapackage body"))?;

        // Collect members that name this contract.
        let adoption_projection = UnitAdoptionProjection::new(&self.conn);
        let mut providers: Vec<MemberSelection> = Vec::new();
        for spec in &body.members {
            if spec.contract.as_ref() != Some(&contract) {
                continue;
            }
            let installed = self.installed_unit(&spec.member_id)?;
            let Some(member_unit) = installed else {
                continue;
            };
            let needs_adoption = member_unit.descriptor.kind.is_content()
                || spec.adoption == MemberAdoption::Attested;
            let adoption_result = if needs_adoption {
                adoption_projection.adoption(
                    &spec.member_id,
                    &member_unit.revision,
                    &member_unit.artifact_digest,
                )?
            } else {
                None
            };
            let (readiness, _) = self.readiness_of(&member_unit)?;
            providers.push(MemberSelection {
                unit_id: spec.member_id.clone(),
                revision: member_unit.revision.clone(),
                artifact_digest: member_unit.artifact_digest.clone(),
                member_kind: member_unit.descriptor.kind,
                adoption: adoption_result,
                enabled: member_unit.enabled,
                readiness,
            });
        }

        let slot = self.build_macro_slot(macro_id, &contract, &providers)?;
        Ok(serde_json::to_value(slot)?)
    }
}
