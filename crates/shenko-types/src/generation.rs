//! Prepared-generation identity.
//!
//! One prepared generation is the single owner of the executable roles a
//! running process offers: native task definitions, provider runtimes and
//! channel plugins, plus their discovery facts. This module is the durable,
//! cross-crate part of that contract:
//!
//! * [`GenerationRef`] names the whole prepared catalog. Its digest is derived
//!   from the catalog content, not from a process-local counter, so an
//!   identical composition composed by a restarted process names the same
//!   content and a changed composition does not.
//! * [`SealedGenerationBinding`] names one role implementation exactly: the
//!   installed unit/revision/artifact when the role is a prepared process unit,
//!   or the compiled implementation reference when the role is compiled into
//!   the running binary, together with the descriptor, export, manifest,
//!   configuration and dependency-closure digests captured during preparation.
//!
//! These identities travel inside sealed requests (a provider binding, a
//! publication intent, a delegated task definition). A replacement generation
//! whose transport or profile metadata happens to be identical therefore still
//! cannot serve work that was sealed against the original bytes: the binding
//! comparison is exact, and a role that is unavailable is refused instead of
//! being silently substituted.
//!
//! The binding is deliberately *not* a runtime handle. Retaining the actual
//! implementations, their execution leases and their retirement is the job of
//! the generation owner; this module only states what the owner selected.
use serde::{Deserialize, Serialize};

use crate::object::{Digest, VersionedRef};

/// Schema of the generation identity types in this module.
pub const PREPARED_GENERATION_SCHEMA: u32 = 1;

/// Which executable role one binding describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparedRole {
    /// A native task definition executed through `NativeTaskHandler`.
    Task,
    /// A provider runtime executed through `ProviderRuntime`.
    Provider,
    /// A channel plugin executed through `ChannelPlugin`.
    Channel,
}

/// The identity of one whole prepared generation.
///
/// `seq` is this process's publication sequence and is diagnostic only. Two
/// generations are the same catalog when their `digest` is equal, which is why
/// every exact comparison in this tree uses [`GenerationRef::same_content`] and
/// never `seq`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationRef {
    /// Monotonic publication sequence in this process. 0 is never published.
    pub seq: u64,
    /// Content digest of the prepared catalog this generation owns.
    pub digest: Digest,
}

impl GenerationRef {
    pub fn new(seq: u64, digest: Digest) -> Self {
        Self { seq, digest }
    }

    /// Whether two references name the same prepared catalog content.
    pub fn same_content(&self, other: &Self) -> bool {
        self.digest == other.digest
    }

    pub fn validate(&self) -> Result<(), GenerationIdentityError> {
        if self.seq == 0 {
            return Err(GenerationIdentityError::Unsequenced);
        }
        if self.digest.canonical_blake3_hex().is_none() {
            return Err(GenerationIdentityError::NonCanonicalDigest {
                field: "generation.digest",
            });
        }
        Ok(())
    }
}

impl std::fmt::Display for GenerationRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "generation:{}:{}", self.seq, self.digest)
    }
}

/// One installed process unit, retained as its immutable identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedUnitIdentity {
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: Digest,
}

impl SealedUnitIdentity {
    pub fn validate(&self) -> Result<(), GenerationIdentityError> {
        if self.unit_id.trim().is_empty() || self.revision.trim().is_empty() {
            return Err(GenerationIdentityError::EmptyField { field: "unit" });
        }
        if self.artifact_digest.canonical_blake3_hex().is_none() {
            return Err(GenerationIdentityError::NonCanonicalDigest {
                field: "unit.artifact_digest",
            });
        }
        Ok(())
    }
}

/// The exact implementation one prepared role selected.
///
/// Either the role is an installed process unit (the adapter decoded its
/// descriptor and staged its bytes), or it is compiled into this binary (its
/// identity is the implementation reference the definition itself publishes).
/// An absent unit is not "unknown": the role is compiled, and the descriptor and
/// closure digests still bind the exact definition it announced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedGenerationBinding {
    /// [`PREPARED_GENERATION_SCHEMA`]; an unreadable version is refused.
    pub schema: u32,
    pub role: PreparedRole,
    /// The exact installed unit, when this role is a prepared process unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<SealedUnitIdentity>,
    /// The exact export id the adapter selected. Empty for compiled roles.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub export_id: String,
    /// The role's own versioned implementation reference.
    pub implementation: VersionedRef,
    /// Digest of the exact manifest that declared the role (process units).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_digest: Option<Digest>,
    /// Digest of the exact descriptor/config bytes captured with the unit.
    pub descriptor_digest: Digest,
    /// Digest of the dependency closure selected for this role.
    pub closure_digest: Digest,
}

impl SealedGenerationBinding {
    /// A role that is an installed process unit.
    pub fn prepared_unit(
        role: PreparedRole,
        unit: SealedUnitIdentity,
        export_id: impl Into<String>,
        implementation: VersionedRef,
        manifest_digest: Digest,
        descriptor_digest: Digest,
        closure_digest: Digest,
    ) -> Self {
        Self {
            schema: PREPARED_GENERATION_SCHEMA,
            role,
            unit: Some(unit),
            export_id: export_id.into(),
            implementation,
            manifest_digest: Some(manifest_digest),
            descriptor_digest,
            closure_digest,
        }
    }

    /// A role compiled into the running binary.
    pub fn compiled(
        role: PreparedRole,
        implementation: VersionedRef,
        descriptor_digest: Digest,
        closure_digest: Digest,
    ) -> Self {
        Self {
            schema: PREPARED_GENERATION_SCHEMA,
            role,
            unit: None,
            export_id: String::new(),
            implementation,
            manifest_digest: None,
            descriptor_digest,
            closure_digest,
        }
    }

    pub fn is_prepared_unit(&self) -> bool {
        self.unit.is_some()
    }

    /// The exact implementation identity this binding names.
    ///
    /// Two bindings are interchangeable only when every captured identity
    /// agrees. The comparison is deliberately structural: an adapter that
    /// rebuilds a binding from current installation metadata must state the same
    /// unit/revision/artifact/config/closure identities, or it is a different
    /// role.
    pub fn matches_exact(&self, other: &Self) -> bool {
        self.schema == other.schema
            && self.role == other.role
            && self.unit == other.unit
            && self.export_id == other.export_id
            && self.implementation == other.implementation
            && self.manifest_digest == other.manifest_digest
            && self.descriptor_digest == other.descriptor_digest
            && self.closure_digest == other.closure_digest
    }

    pub fn validate(&self) -> Result<(), GenerationIdentityError> {
        if self.schema != PREPARED_GENERATION_SCHEMA {
            return Err(GenerationIdentityError::Schema {
                found: self.schema,
            });
        }
        if let Some(unit) = &self.unit {
            unit.validate()?;
        }
        if self.unit.is_some() && self.export_id.trim().is_empty() {
            return Err(GenerationIdentityError::EmptyField { field: "export_id" });
        }
        if self.unit.is_none() && !self.export_id.is_empty() {
            return Err(GenerationIdentityError::UnexpectedExport);
        }
        if self.implementation.symbol.trim().is_empty() {
            return Err(GenerationIdentityError::EmptyField {
                field: "implementation.symbol",
            });
        }
        if self.descriptor_digest.canonical_blake3_hex().is_none() {
            return Err(GenerationIdentityError::NonCanonicalDigest {
                field: "descriptor_digest",
            });
        }
        if self.closure_digest.canonical_blake3_hex().is_none() {
            return Err(GenerationIdentityError::NonCanonicalDigest {
                field: "closure_digest",
            });
        }
        if let Some(manifest) = &self.manifest_digest {
            if manifest.canonical_blake3_hex().is_none() {
                return Err(GenerationIdentityError::NonCanonicalDigest {
                    field: "manifest_digest",
                });
            }
            if self.unit.is_none() {
                return Err(GenerationIdentityError::UnexpectedManifest);
            }
        }
        Ok(())
    }
}

impl std::fmt::Display for SealedGenerationBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.unit {
            Some(unit) => write!(
                f,
                "{}:{}@{}#{}",
                match self.role {
                    PreparedRole::Task => "task",
                    PreparedRole::Provider => "provider",
                    PreparedRole::Channel => "channel",
                },
                unit.unit_id,
                unit.revision,
                unit.artifact_digest
            ),
            None => write!(
                f,
                "{}:compiled:{}",
                match self.role {
                    PreparedRole::Task => "task",
                    PreparedRole::Provider => "provider",
                    PreparedRole::Channel => "channel",
                },
                self.implementation.symbol
            ),
        }
    }
}

/// A refused single-shot attachment of a prepared generation port.
///
/// Both the provider registry and the channel registry accept exactly one
/// prepared source for the life of a process. The identity rule is the same in
/// both places: a source must publish a generation, re-attaching the exact
/// same source is idempotent, and a different source may not replace the
/// installed port even when its generation content happens to be identical.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PreparedAttachmentError {
    /// A different attachment already owns the port.
    #[error("a prepared source is already attached")]
    AlreadyAttached,
    /// The source has no published generation, so it cannot be authoritative.
    #[error("no prepared generation is published")]
    NoGenerationPublished,
    /// Another generation is already installed.
    #[error("prepared source for generation {incoming:?} replaces the attached generation {attached:?}")]
    DifferentGeneration {
        /// The generation the installed port already names.
        attached: Option<GenerationRef>,
        /// The generation the refused source publishes.
        incoming: Option<GenerationRef>,
    },
}

impl PreparedAttachmentError {
    /// Check one attachment without changing the port.
    ///
    /// `same_source` is `true` only when the incoming source is the exact
    /// allocation already attached. Identical generation content from another
    /// source is not the same port: it is [`Self::AlreadyAttached`] when the
    /// full reference agrees and [`Self::DifferentGeneration`] otherwise.
    pub fn check(
        same_source: bool,
        attached: Option<GenerationRef>,
        incoming: Option<GenerationRef>,
    ) -> Result<(), Self> {
        let Some(incoming) = incoming else {
            return Err(Self::NoGenerationPublished);
        };
        if same_source {
            return Ok(());
        }
        match attached {
            None => Ok(()),
            Some(attached) if attached == incoming => Err(Self::AlreadyAttached),
            Some(attached) => Err(Self::DifferentGeneration {
                attached: Some(attached),
                incoming: Some(incoming),
            }),
        }
    }
}

/// A malformed generation identity. These are refusals, never repairs: a
/// binding that cannot be read exactly is not reinterpreted as a looser one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GenerationIdentityError {
    #[error("prepared generation schema {found} is not supported")]
    Schema { found: u32 },
    #[error("prepared generation has no publication sequence")]
    Unsequenced,
    #[error("generation identity field {field} is empty")]
    EmptyField { field: &'static str },
    #[error("generation identity field {field} is not a canonical blake3 digest")]
    NonCanonicalDigest { field: &'static str },
    #[error("a compiled role must not name an export")]
    UnexpectedExport,
    #[error("a compiled role must not name a manifest digest")]
    UnexpectedManifest,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{ArtifactRef, SchemaRef};

    fn reference() -> VersionedRef {
        let digest = Digest::blake3(b"compiled-definition");
        VersionedRef {
            artifact: ArtifactRef {
                digest: digest.clone(),
                locator: "native:fixture".into(),
            },
            symbol: "native.fixture.execute".into(),
            schema: SchemaRef {
                name: "native-task/fixture".into(),
                major: 1,
                digest,
            },
        }
    }

    #[test]
    fn compiled_and_prepared_bindings_validate_and_differ() {
        let compiled = SealedGenerationBinding::compiled(
            PreparedRole::Task,
            reference(),
            Digest::blake3(b"descriptor"),
            Digest::blake3(b"closure"),
        );
        compiled.validate().unwrap();
        let unit = SealedGenerationBinding::prepared_unit(
            PreparedRole::Task,
            SealedUnitIdentity {
                unit_id: "process".into(),
                revision: "1.0.0".into(),
                artifact_digest: Digest::blake3(b"unit"),
            },
            "fixture.task",
            reference(),
            Digest::blake3(b"manifest"),
            Digest::blake3(b"descriptor"),
            Digest::blake3(b"closure"),
        );
        unit.validate().unwrap();
        assert!(!compiled.matches_exact(&unit));
    }

    #[test]
    fn a_changed_captured_digest_is_a_different_role() {
        let base = SealedGenerationBinding::compiled(
            PreparedRole::Provider,
            reference(),
            Digest::blake3(b"descriptor"),
            Digest::blake3(b"closure"),
        );
        let mut changed = base.clone();
        // The descriptor digest carries the captured configuration; a
        // different captured configuration is a different role identity.
        changed.descriptor_digest = Digest::blake3(b"descriptor-2");
        assert!(!base.matches_exact(&changed));
    }

    #[test]
    fn generation_content_comparison_ignores_the_process_sequence() {
        let digest = Digest::blake3(b"catalog");
        let first = GenerationRef::new(1, digest.clone());
        let restarted = GenerationRef::new(7, digest);
        first.validate().unwrap();
        assert!(first.same_content(&restarted));
        assert_ne!(first, restarted);
    }

    /// The shared attachment fence: one published port per process, and an
    /// identical catalog attached by another process is not the same port.
    #[test]
    fn prepared_attachment_requires_one_published_generation() {
        let digest = Digest::blake3(b"catalog");
        let first = GenerationRef::new(1, digest.clone());
        let restarted = GenerationRef::new(7, digest.clone());
        let other = GenerationRef::new(2, Digest::blake3(b"other"));
        assert_eq!(
            PreparedAttachmentError::check(false, None, None),
            Err(PreparedAttachmentError::NoGenerationPublished)
        );
        assert_eq!(
            PreparedAttachmentError::check(false, None, Some(first.clone())),
            Ok(())
        );
        assert_eq!(
            PreparedAttachmentError::check(false, Some(first.clone()), Some(first.clone())),
            Err(PreparedAttachmentError::AlreadyAttached)
        );
        assert_eq!(
            PreparedAttachmentError::check(true, Some(first.clone()), Some(first.clone())),
            Ok(())
        );
        // The sequence is diagnostic: matching content from another process is
        // still a different attachment of the same catalog.
        assert_eq!(
            PreparedAttachmentError::check(false, Some(first.clone()), Some(restarted)),
            Err(PreparedAttachmentError::AlreadyAttached)
        );
        assert!(matches!(
            PreparedAttachmentError::check(false, Some(first), Some(other)),
            Err(PreparedAttachmentError::DifferentGeneration { .. })
        ));
    }
}


/// Immutable catalog reference; neither its digest nor publication sequence is
/// permission to construct a runtime. Only an owner-admitted original can serve it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenGenerationSnapshot {
    pub generation: GenerationRef,
    pub catalog_ref: crate::object::ObjectRef,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenTaskRole {
    pub kind: String,
    pub definition_version: u32,
    pub implementation: crate::task::ImplementationRef,
    pub binding: SealedGenerationBinding,
    /// Exact descriptor, prompt, dialogue support, capability and effect facts
    /// read from the prepared original, retained in the CAS catalog closure.
    pub facts: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenChannelRole {
    pub channel: String,
    pub sources: Vec<String>,
    pub binding: SealedGenerationBinding,
    pub facts: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenReaderRole {
    pub task: FrozenTaskRole,
    pub contract_digest: Digest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenProviderRole {
    pub binding: SealedGenerationBinding,
    /// Exact transport/profile metadata of the admitted runtime, as already
    /// represented by the request binding; this contains no runtime handles.
    pub transport: serde_json::Value,
}

/// The complete immutable facts used by this S0. Readers remain a separate
/// inventory; they never satisfy a fresh task lookup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenGenerationCatalog {
    pub schema: u32,
    pub generation: GenerationRef,
    pub tasks: Vec<FrozenTaskRole>,
    pub providers: Vec<FrozenProviderRole>,
    pub channels: Vec<FrozenChannelRole>,
    pub readers: Vec<FrozenReaderRole>,
    pub prompt_sections: Vec<String>,
}

impl FrozenGenerationCatalog {
    pub fn validate(&self) -> Result<(), GenerationIdentityError> {
        if self.schema != PREPARED_GENERATION_SCHEMA {
            return Err(GenerationIdentityError::Schema { found: self.schema });
        }
        self.generation.validate()?;
        for (index, task) in self.tasks.iter().enumerate() {
            task.validate()?;
            if self.tasks[..index].iter().any(|other|
                other.kind == task.kind && other.definition_version == task.definition_version)
            { return Err(GenerationIdentityError::EmptyField { field: "duplicate task role" }); }
        }
        for provider in &self.providers {
            provider.binding.validate()?;
            if provider.binding.role != PreparedRole::Provider {
                return Err(GenerationIdentityError::EmptyField { field: "provider role" });
            }
        }
        for (index, channel) in self.channels.iter().enumerate() {
            channel.binding.validate()?;
            if channel.channel.is_empty() || channel.binding.role != PreparedRole::Channel
                || self.channels[..index].iter().any(|other| other.channel == channel.channel)
            { return Err(GenerationIdentityError::EmptyField { field: "channel role" }); }
        }
        for reader in &self.readers {
            reader.task.validate()?;
            if reader.contract_digest.canonical_blake3_hex().is_none() {
                return Err(GenerationIdentityError::NonCanonicalDigest { field: "reader.contract_digest" });
            }
        }
        Ok(())
    }

    pub fn task(&self, kind: &str, version: u32) -> Option<&FrozenTaskRole> {
        self.tasks.iter().find(|task| task.kind == kind && task.definition_version == version)
    }

    pub fn channel(&self, channel: &str) -> Option<&FrozenChannelRole> {
        self.channels.iter().find(|role| role.channel == channel)
    }
}

impl FrozenTaskRole {
    fn validate(&self) -> Result<(), GenerationIdentityError> {
        self.binding.validate()?;
        if self.kind.is_empty() || self.definition_version == 0 || self.binding.role != PreparedRole::Task
            || self.binding.implementation.artifact.digest != self.implementation.artifact_digest
            || self.facts.get("kind").and_then(serde_json::Value::as_str) != Some(self.kind.as_str())
            || self.facts.get("definition_version").and_then(serde_json::Value::as_u64) != Some(u64::from(self.definition_version))
            || self.facts.get("implementation") != serde_json::to_value(&self.implementation).ok().as_ref()
        { return Err(GenerationIdentityError::EmptyField { field: "task descriptor identity" }); }
        Ok(())
    }
}
