//! Typed package requirements, capability contracts and canonical declaration documents.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;
use super::codec::{UnitDescriptor, UnitKind};

// ------------------------------------------------- package declarations ---

/// One guest-visible filesystem requirement of a package
/// "权限需求": a guest path the executor must preopen, and whether the
/// package writes under it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct FileRequirement {
    /// Absolute guest path, for example `/home` or `/home/work`.
    pub guest_path: String,
    /// True when the package writes under the path, false for read-only use.
    pub write: bool,
}

/// One socket operation a package needs.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkOperation {
    Connect {},
    Listen {},
    Accept {},
}

impl NetworkOperation {
    pub fn as_str(&self) -> &'static str {
        match self {
            NetworkOperation::Connect {} => "connect",
            NetworkOperation::Listen {} => "listen",
            NetworkOperation::Accept {} => "accept",
        }
    }
}

/// One network target a package declares it needs, in the same typed shape the
/// network transport admits. A requirement is matched against an admitted
/// target; the transport never invents one.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct NetworkTarget {
    pub host: String,
    pub port: u16,
    /// True when the guest asks the host to resolve the name; false when it
    /// connects to the literal host.
    #[serde(default)]
    pub resolve: bool,
    /// The operations the package needs on this target.
    pub operations: Vec<NetworkOperation>,
}

/// A host facility a package can require beyond files, arguments, environment
/// variables and the three standard streams.
///
/// The variants are zero-field structs rather than unit variants so that
/// `deny_unknown_fields` rejects an unexpected key on every one of them
/// (serde routes internally tagged unit variants through an ignoring visitor).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "facility", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostRequirement {
    /// Wall-clock and monotonic clock calls.
    Clock {},
    /// Entropy (`random_get`).
    Random {},
    /// Socket access to these exact targets. A package that declares this and
    /// receives no admitted target set is unsupported, never run under an
    /// invented empty admission.
    Network { targets: Vec<NetworkTarget> },
    /// One named host import module other than `wasi_snapshot_preview1`.
    HostImport { module: String },
}

/// The capability requirements one package version declares. They are
/// requirements, never grants: a declaration is admitted against the resources
/// the runtime actually hands the guest, and a declaration is never permission
/// to reach anything outside that grant.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageCapabilities {
    #[serde(default)]
    pub filesystem: Vec<FileRequirement>,
    /// Environment variable names that must be supplied per invocation.
    #[serde(default)]
    pub environment: Vec<String>,
    #[serde(default)]
    pub host: Vec<HostRequirement>,
    /// The atomic capabilities this code version actually exercises at run
    /// time (hook points, event subscriptions, singleton services).
    ///
    /// They are what coexistence is judged on: providers may be installed and
    /// enabled together unless both need the same atom exclusively (SPEC
    ///). There is deliberately no package-level `conflicts` field.
    #[serde(default)]
    pub atoms: Vec<AtomicCapability>,
}

/// Whether one package version records its capability requirements.
///
/// A package written before declarations existed carries none. That absence is
/// not "needs nothing" and never a blanket grant: the runtime keeps the
/// profile's own grant and reports the declaration as unrecorded, so an old
/// artifact neither loses access it had nor gains any.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageDeclaration {
    /// `None` when the unit version was written before declarations existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared: Option<PackageCapabilities>,
}

impl PackageDeclaration {
    /// A version that records no declaration.
    pub fn unrecorded() -> Self {
        Self::default()
    }

    pub fn declared(capabilities: PackageCapabilities) -> Self {
        Self {
            declared: Some(capabilities),
        }
    }

    pub fn is_unrecorded(&self) -> bool {
        self.declared.is_none()
    }

    /// `"declared"` or `"unrecorded"`, for receipts and projections.
    pub fn state(&self) -> &'static str {
        if self.declared.is_some() {
            "declared"
        } else {
            "unrecorded"
        }
    }

    pub fn capabilities(&self) -> Option<&PackageCapabilities> {
        self.declared.as_ref()
    }

    /// The declaration in canonical order, so two equal declarations are one
    /// document for digests and divergence reports.
    pub fn normalized(&self) -> Self {
        match &self.declared {
            Some(capabilities) => Self::declared(capabilities.normalized()),
            None => Self::unrecorded(),
        }
    }
}

/// Bounds on one declaration. They bound memory and the admission walk; none
/// of them is a product permission decision.
pub const MAX_DECLARED_PATHS: usize = 32;
pub const MAX_DECLARED_ENVIRONMENT: usize = 32;
pub const MAX_DECLARED_HOST: usize = 16;
/// Bound on the targets one network requirement may name.
pub const MAX_DECLARED_NETWORK_TARGETS: usize = 16;
/// Guest path and environment-name length bound. Long enough for a real
/// managed-home layout, small enough to keep a generation document bounded.
pub const MAX_CAPABILITY_NAME_BYTES: usize = 256;

impl PackageCapabilities {
    /// Typed admission of the declaration itself: bounded counts, absolute
    /// normalized guest paths, environment variable names and host facilities.
    /// Nothing here consults a model, a human or a keyword list.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.filesystem.len() <= MAX_DECLARED_PATHS,
            "TOOL_CAPABILITY_INVALID: at most {MAX_DECLARED_PATHS} declared filesystem paths"
        );
        ensure!(
            self.environment.len() <= MAX_DECLARED_ENVIRONMENT,
            "TOOL_CAPABILITY_INVALID: at most {MAX_DECLARED_ENVIRONMENT} declared environment variables"
        );
        ensure!(
            self.host.len() <= MAX_DECLARED_HOST,
            "TOOL_CAPABILITY_INVALID: at most {MAX_DECLARED_HOST} declared host facilities"
        );
        for requirement in &self.filesystem {
            validate_guest_path(&requirement.guest_path)?;
        }
        for name in &self.environment {
            ensure!(
                !name.is_empty()
                    && name.len() <= MAX_CAPABILITY_NAME_BYTES
                    && name
                        .chars()
                        .enumerate()
                        .all(|(index, c)| if index == 0 {
                            c.is_ascii_alphabetic() || c == '_'
                        } else {
                            c.is_ascii_alphanumeric() || c == '_'
                        }),
                "TOOL_CAPABILITY_INVALID: environment name {name:?} must match \
                 [A-Za-z_][A-Za-z0-9_]*"
            );
        }
        for requirement in &self.host {
            match requirement {
                HostRequirement::HostImport { module } => {
                    ensure!(
                        !module.is_empty()
                            && module.len() <= MAX_CAPABILITY_NAME_BYTES
                            && module != "wasi_snapshot_preview1",
                        "TOOL_CAPABILITY_INVALID: host import module {module:?} is not a \
                         declared requirement beyond wasi_snapshot_preview1"
                    );
                }
                HostRequirement::Network { targets } => {
                    ensure!(
                        !targets.is_empty(),
                        "TOOL_CAPABILITY_INVALID: a network requirement must name the \
                         targets it needs; there is no default target set"
                    );
                    ensure!(
                        targets.len() <= MAX_DECLARED_NETWORK_TARGETS,
                        "TOOL_CAPABILITY_INVALID: at most {MAX_DECLARED_NETWORK_TARGETS} \
                         declared network targets"
                    );
                    for target in targets {
                        ensure!(
                            !target.host.is_empty()
                                && target.host.len() <= 253
                                && !target
                                    .host
                                    .chars()
                                    .any(|c| c.is_whitespace() || c.is_control()),
                            "TOOL_CAPABILITY_INVALID: network target host {:?} is not a \
                             host name or address",
                            target.host
                        );
                        ensure!(
                            !target.operations.is_empty(),
                            "TOOL_CAPABILITY_INVALID: network target {}:{} declares no \
                             operation",
                            target.host,
                            target.port
                        );
                    }
                }
                HostRequirement::Clock {} | HostRequirement::Random {} => {}
            }
        }
        ensure!(
            self.atoms.len() <= MAX_ATOMIC_CAPABILITIES,
            "TOOL_CAPABILITY_INVALID: at most {MAX_ATOMIC_CAPABILITIES} declared atomic capabilities"
        );
        let mut atoms: Vec<&str> = Vec::new();
        for atom in &self.atoms {
            validate_atom_id(&atom.atom_id)?;
            ensure!(
                !atoms.contains(&atom.atom_id.as_str()),
                "TOOL_CAPABILITY_INVALID: atomic capability {} is declared twice",
                atom.atom_id
            );
            atoms.push(&atom.atom_id);
        }
        Ok(())
    }

    /// The declaration in one stable order, so two equal declarations produce
    /// equal documents and equal closure digests.
    pub fn normalized(&self) -> Self {
        let mut normalized = self.clone();
        normalized.filesystem.sort();
        normalized.filesystem.dedup();
        normalized.environment.sort();
        normalized.environment.dedup();
        for requirement in &mut normalized.host {
            if let HostRequirement::Network { targets } = requirement {
                for target in targets.iter_mut() {
                    target.operations.sort();
                    target.operations.dedup();
                }
                targets.sort();
                targets.dedup();
            }
        }
        normalized.host.sort();
        normalized.host.dedup();
        normalized.atoms.sort();
        normalized.atoms.dedup();
        normalized
    }
}

/// A guest path a package may name: absolute, normalized, bounded.
fn validate_guest_path(path: &str) -> Result<()> {
    ensure!(
        path.starts_with('/') && path.len() <= MAX_CAPABILITY_NAME_BYTES,
        "TOOL_CAPABILITY_INVALID: guest path {path:?} must be absolute and at most \
         {MAX_CAPABILITY_NAME_BYTES} bytes"
    );
    ensure!(
        !path.contains('\0'),
        "TOOL_CAPABILITY_INVALID: guest path contains NUL"
    );
    let body = path.trim_start_matches('/');
    if !body.is_empty() {
        ensure!(
            !body.ends_with('/'),
            "TOOL_CAPABILITY_INVALID: guest path {path:?} must not end with '/'"
        );
        for component in body.split('/') {
            ensure!(
                !component.is_empty() && component != "." && component != "..",
                "TOOL_CAPABILITY_INVALID: guest path {path:?} is not normalized"
            );
        }
    }
    Ok(())
}

// ------------------------------------ capability contracts ---

/// Reserved prefix of a capability contract identifier.
///
/// A capability contract and a unit id are two identifier spaces: a unit id may
/// never begin with this prefix, and a dependency written on a contract is
/// satisfied by whichever installed unit declares providing it.
pub const CAPABILITY_CONTRACT_PREFIX: &str = "cap.";
/// Bound on the atomic capabilities one code version declares.
pub const MAX_ATOMIC_CAPABILITIES: usize = 32;
/// Bound on a capability contract name.
pub const MAX_CONTRACT_BYTES: usize = 128;

fn validate_dotted_name(name: &str, what: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.len() <= MAX_CONTRACT_BYTES,
        "UNIT_CAPABILITY_INVALID: {what} {name:?} must be 1..{MAX_CONTRACT_BYTES} bytes"
    );
    for segment in name.split('.') {
        ensure!(
            !segment.is_empty(),
            "UNIT_CAPABILITY_INVALID: {what} {name:?} has an empty segment"
        );
        ensure!(
            segment
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'),
            "UNIT_CAPABILITY_INVALID: {what} {name:?} must be lowercase ASCII letters, digits, '-' or '_' in dot-separated segments"
        );
    }
    Ok(())
}

/// One versioned capability contract, written `cap.<name>/<major>`.
///
/// The contract identity is the pair (name, major): a provider's own package
/// version may change freely, and only a different contract major version makes
/// a dependency unsatisfied. The value serializes as its text form,
/// so an author writes one string and no field can drift from it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CapabilityContract {
    /// `cap.` followed by a dotted name, for example `cap.forgetting`.
    pub contract_id: String,
    /// Major version, at least 1.
    pub major: u32,
}

impl CapabilityContract {
    /// Parse the text form. A malformed contract is refused instead of being
    /// treated as an opaque topic.
    pub fn parse(text: &str) -> Result<Self> {
        let trimmed = text.trim();
        let Some(rest) = trimmed.strip_prefix(CAPABILITY_CONTRACT_PREFIX) else {
            bail!(
                "UNIT_CAPABILITY_INVALID: capability contract {text:?} must begin with {CAPABILITY_CONTRACT_PREFIX:?}"
            );
        };
        let Some((name, version)) = rest.rsplit_once('/') else {
            bail!(
                "UNIT_CAPABILITY_INVALID: capability contract {text:?} must be `cap.<name>/<major>`"
            );
        };
        validate_dotted_name(name, "capability contract name")?;
        ensure!(
            !version.is_empty()
                && version.len() <= 9
                && version.chars().all(|c| c.is_ascii_digit()),
            "UNIT_CAPABILITY_INVALID: capability contract {text:?} major version must be a decimal number"
        );
        let major: u32 = version.parse().with_context(|| {
            format!("UNIT_CAPABILITY_INVALID: capability contract {text:?} major version is out of range")
        })?;
        ensure!(
            major >= 1,
            "UNIT_CAPABILITY_INVALID: capability contract {text:?} starts at major version 1"
        );
        Ok(Self {
            contract_id: format!("{CAPABILITY_CONTRACT_PREFIX}{name}"),
            major,
        })
    }

    /// Build a contract from a name with or without the reserved prefix.
    pub fn new(name: &str, major: u32) -> Result<Self> {
        let text = if name.starts_with(CAPABILITY_CONTRACT_PREFIX) {
            format!("{name}/{major}")
        } else {
            format!("{CAPABILITY_CONTRACT_PREFIX}{name}/{major}")
        };
        Self::parse(&text)
    }

    /// The text form, `cap.<name>/<major>`.
    pub fn text(&self) -> String {
        format!("{}/{}", self.contract_id, self.major)
    }

    /// Whether a reference names a typed capability contract at all. Every
    /// other `provides` entry is a topic or capability name.
    pub fn is_contract_ref(text: &str) -> bool {
        text.starts_with(CAPABILITY_CONTRACT_PREFIX)
    }

    /// Whether a provider of `provided` satisfies this requirement. Same name
    /// and same major version; nothing else.
    pub fn matches(&self, provided: &Self) -> bool {
        self.contract_id == provided.contract_id && self.major == provided.major
    }
}

impl fmt::Display for CapabilityContract {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text())
    }
}

impl Serialize for CapabilityContract {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text())
    }
}

impl<'de> Deserialize<'de> for CapabilityContract {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// Whether a provider needs an atomic capability to itself or shares it.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityExclusivity {
    /// Several providers may hold this capability at the same time.
    #[default]
    Shared,
    /// Two enabled providers cannot both hold it.
    Exclusive,
}

/// One atomic capability a code version actually exercises at run time: a hook
/// point, an event subscription, a singleton service.
///
/// Coexistence is judged here, on what is actually used. Packages
/// carry no `conflicts` field and there is no package-level exclusivity.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct AtomicCapability {
    pub atom_id: String,
    #[serde(default)]
    pub exclusivity: CapabilityExclusivity,
}

fn validate_atom_id(atom_id: &str) -> Result<()> {
    ensure!(
        !atom_id.is_empty() && atom_id.len() <= MAX_CONTRACT_BYTES,
        "TOOL_CAPABILITY_INVALID: atomic capability {atom_id:?} must be 1..{MAX_CONTRACT_BYTES} bytes"
    );
    ensure!(
        atom_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':')),
        "TOOL_CAPABILITY_INVALID: atomic capability {atom_id:?} must be ASCII letters, digits, '.', '_', '-' or ':'"
    );
    Ok(())
}

/// The atomic capability two enabled providers both need exclusively, if any.
///
/// Providers that only share capabilities coexist. Nothing here consults a
/// model, a human or a package-level declaration.
pub fn atomic_conflict(left: &[AtomicCapability], right: &[AtomicCapability]) -> Option<String> {
    for atom in left
        .iter()
        .filter(|atom| atom.exclusivity == CapabilityExclusivity::Exclusive)
    {
        if right.iter().any(|other| {
            other.exclusivity == CapabilityExclusivity::Exclusive && other.atom_id == atom.atom_id
        }) {
            return Some(atom.atom_id.clone());
        }
    }
    None
}

/// The capability requirements one unit version authored, read from the
/// package description that travels with it.
///
/// A code package carries its description under `config.tool`; the
/// `capabilities` object there is the declaration for every
/// package description to record. A version without that object records no
/// declaration: that is [`PackageDeclaration::unrecorded`], which the runtime
/// reports as such — never a fabricated empty declaration (which would claim
/// the package needs nothing) and never a blanket grant.
///
/// A malformed declaration refuses the closure it belongs to, so an
/// environment never pins requirements the node cannot even parse.
pub fn declared_package_capabilities(descriptor: &UnitDescriptor) -> Result<PackageDeclaration> {
    if descriptor.kind != UnitKind::Code {
        return Ok(PackageDeclaration::unrecorded());
    }
    let Some(value) = descriptor.config.pointer("/tool/capabilities") else {
        return Ok(PackageDeclaration::unrecorded());
    };
    let declaration: PackageDeclaration =
        serde_json::from_value(value.clone()).with_context(|| {
            format!(
                "UNIT_PACKAGE_CAPABILITIES_INVALID: {} revision {} declares unreadable capabilities",
                descriptor.unit_id, descriptor.revision
            )
        })?;
    if let Some(capabilities) = declaration.capabilities() {
        capabilities.validate()?;
    }
    // The frozen copy is normalized so two equal declarations are one
    // document: the closure digest and the divergence report both compare
    // declarations, not authoring order.
    Ok(declaration.normalized())
}

/// The canonical JSON one declaration is hashed and reported as. An
/// unrecorded declaration is its own state, not an empty declaration.
pub fn declaration_document(declarations: &PackageDeclaration) -> Value {
    match declarations.capabilities() {
        Some(capabilities) => json!({
            "state": "declared",
            "capabilities": capabilities.normalized(),
        }),
        None => json!({"state": "unrecorded"}),
    }
}
