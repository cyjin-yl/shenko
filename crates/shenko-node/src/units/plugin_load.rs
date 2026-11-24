//! Runtime loading of installed code plugins from the Node installation owner.
//!
//! A process may only prepare executable plugin code from a record this node
//! actually owns. This module is that read boundary: it resolves the exact
//! installed version, re-hashes the artifact bytes against the retained
//! digest, binds the result to the active environment generation when one is
//! published, and returns an immutable image plus a receipt naming every link
//! of that chain. It creates no process, service, capability or readiness
//! fact and writes nothing.
//!
//! The installation tables are the only authority:
//!
//! * `unit_current` names the version this installation currently offers and
//!   whether the unit is enabled;
//! * `unit_versions` retains the exact `StoredVersion` (descriptor, config and
//!   edges) for one `(unit_id, revision, artifact_digest)`;
//! * `unit_environment.selector` + `unit_environment.generation.<n>` name the
//!   active generation and the exact version it pins;
//! * `objects` holds the inline artifact bytes keyed by their blake3 digest,
//!   which this loader re-hashes instead of trusting the key.
//!
//! `unit_materializations` is a copy for transparency and is never consulted.
//! An out-of-line locator is refused rather than dereferenced, because Node
//! does not own that store. A withdrawn version is gone from `unit_versions`,
//! so it is refused as "not retained" instead of being revived from a cache.
//!
//! Refusals are stable identifiers:
//!
//! * `PLUGIN_LOAD_RECORD` — the named version is not retained here.
//! * `PLUGIN_LOAD_IDENTITY` — a malformed exact identity.
//! * `PLUGIN_LOAD_KIND` — the unit is not executable code.
//! * `PLUGIN_LOAD_DISABLED` — a live selection is not enabled.
//! * `PLUGIN_LOAD_ENVIRONMENT` — the caller required a generation and none is
//!   active.
//! * `PLUGIN_LOAD_NOT_SELECTED` — the active generation does not pin the unit.
//! * `PLUGIN_LOAD_GENERATION` — the caller's generation witness differs from
//!   the active selector, or a conditional current load disagrees with the
//!   generation's pin.
//! * `PLUGIN_LOAD_ARTIFACT` — the bytes are out of line or not retained.
//! * `PLUGIN_LOAD_HASH` — the retained bytes do not hash to the record.
//! * `PLUGIN_LOAD_BINDING` — the descriptor's artifact declaration disagrees
//!   with the retained identity.
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use shenko_types::generation::SealedUnitIdentity;
use shenko_types::object::Digest;

use super::codec::{
    MAX_ARTIFACT_BYTES, UnitArtifactSource, UnitDescriptor, UnitKind, external_artifact_of,
};
use super::environment::{EnvironmentPin, EnvironmentSelector, read_environment_selector};
use crate::Node;

/// Schema of [`PluginLoadReceipt`]; an unreadable version is refused by its
/// readers, never reinterpreted as a looser receipt.
pub const PLUGIN_LOAD_RECEIPT_SCHEMA: u32 = 1;

/// The active environment generation a caller observed before asking for a
/// plugin. Passing it makes the load conditional: a selector or pin that moved
/// in between is a refusal, not a silently newer image.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginGenerationWitness {
    pub generation: u64,
    /// The closure digest of that generation.
    pub digest: Digest,
    /// The selector revision that published it.
    pub selector_revision: u64,
}

impl PluginGenerationWitness {
    pub fn of_selector(selector: &EnvironmentSelector) -> Self {
        Self {
            generation: selector.generation,
            digest: selector.generation_digest.clone(),
            selector_revision: selector.revision,
        }
    }

    pub fn of_pin(pin: &EnvironmentPin) -> Self {
        Self {
            generation: pin.generation,
            digest: pin.generation_digest.clone(),
            selector_revision: pin.selector_revision,
        }
    }
}

/// Which retained version the runtime asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginLoadSelection {
    /// The exact version the active environment generation pins. Refused when
    /// no generation is active or the generation does not pin this unit.
    Environment {
        expected: Option<PluginGenerationWitness>,
    },
    /// The version `unit_current` names for this unit — what a fresh,
    /// non-generation-bound caller uses. Reports the pin only when the current
    /// version happens to be the generation's pin.
    Current {
        expected: Option<PluginGenerationWitness>,
    },
    /// One exact retained version, for work that was frozen against it (a
    /// [`shenko_types::generation::SealedGenerationBinding`] names exactly
    /// this identity). A retained version stays loadable while its bytes are
    /// held; enablement and current state are reported, not invented.
    Exact {
        identity: SealedUnitIdentity,
        expected: Option<PluginGenerationWitness>,
    },
}

/// Which rule selected the loaded version, recorded in the receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginSelectionKind {
    Environment,
    Current,
    Exact,
}

/// The verified authority chain of one loaded plugin image.
///
/// The receipt is a read result, not another installation record: it is
/// derived from the durable tables and can be recomputed, but it is never
/// written back. `descriptor_digest` and `content_digest` are the digests of
/// the exact documents the loader verified, so a caller can compare the
/// receipt it prepared against the one a re-read produces.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginLoadReceipt {
    pub schema: u32,
    pub unit_id: String,
    pub kind: UnitKind,
    pub revision: String,
    /// The unit-record artifact identity (the key the inventory uses).
    pub artifact_digest: Digest,
    /// blake3 of the exact retained `UnitDescriptor` document.
    pub descriptor_digest: Digest,
    /// blake3 of the artifact bytes this load actually read. Equal to
    /// `artifact_digest` for every admitted inline artifact.
    pub content_digest: Digest,
    pub bytes: u64,
    pub artifact_source: UnitArtifactSource,
    pub selection: PluginSelectionKind,
    /// The generation pin that locked this exact version, when one does.
    pub environment: Option<EnvironmentPin>,
    /// Whether `unit_current` names this exact version.
    pub current: bool,
    /// Whether the installation has this unit enabled.
    pub unit_enabled: bool,
}

impl PluginLoadReceipt {
    /// A digest over the receipt document, so two preparations of the same
    /// installation facts compare equal and any changed link differs.
    pub fn digest(&self) -> Result<Digest> {
        Ok(Digest::blake3(&serde_json::to_vec(&(
            "node.plugin-load-receipt/v1",
            self,
        ))?))
    }
}

/// One verified, executable plugin image.
///
/// The bytes are immutable and shared, so a runtime generation owner can hand
/// the same image to more than one worker without copying it. Nothing here is
/// a handle: the owner still creates, owns and retires whatever it prepares
/// from these bytes.
#[derive(Debug)]
pub struct InstalledPluginCode {
    receipt: PluginLoadReceipt,
    descriptor: UnitDescriptor,
    bytes: Arc<[u8]>,
}

impl InstalledPluginCode {
    pub fn receipt(&self) -> &PluginLoadReceipt {
        &self.receipt
    }

    /// The exact retained descriptor, including the adapter configuration the
    /// package authored. Configuration never supplies executable identity or
    /// an execution grant.
    pub fn descriptor(&self) -> &UnitDescriptor {
        &self.descriptor
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Arc<[u8]> {
        self.bytes
    }
}

impl Node {
    /// Load one installed code plugin's exact artifact bytes.
    ///
    /// Every path re-hashes the bytes against the retained digest and reports
    /// the generation that pins the version. Nothing is created, published or
    /// written; `self` is only read.
    pub fn load_installed_plugin_code(
        &self,
        unit_id: &str,
        selection: PluginLoadSelection,
    ) -> Result<InstalledPluginCode> {
        ensure!(
            !unit_id.trim().is_empty(),
            "PLUGIN_LOAD_RECORD: unit identity is empty"
        );
        let selector = read_environment_selector(&self.conn)?;
        let members = match &selector {
            Some(selector) => {
                let generation = self
                    .environment_generation(selector.generation)?
                    .with_context(|| {
                        format!(
                            "PLUGIN_LOAD_ENVIRONMENT: the active selector names generation {} but \
                             it is not recorded",
                            selector.generation
                        )
                    })?;
                ensure!(
                    generation.generation == selector.generation
                        && generation.digest == selector.generation_digest,
                    "PLUGIN_LOAD_ENVIRONMENT: the active environment selector and its recorded \
                     generation disagree"
                );
                generation.members
            }
            None => Vec::new(),
        };
        let witness = |expected: &PluginGenerationWitness| -> Result<()> {
            let active = selector.as_ref().context(
                "PLUGIN_LOAD_ENVIRONMENT: this installation has no active environment generation; \
                 load by current version or exact retained identity, without a generation witness",
            )?;
            ensure!(
                active.generation == expected.generation
                    && active.generation_digest == expected.digest
                    && active.revision == expected.selector_revision,
                "PLUGIN_LOAD_GENERATION: the active environment changed (generation {} {} at selector \
                 revision {}), not the observed generation {} {} at revision {}",
                active.generation,
                active.generation_digest,
                active.revision,
                expected.generation,
                expected.digest,
                expected.selector_revision
            );
            Ok(())
        };
        let pin_of = |unit_id: &str, revision: &str, digest: &Digest| -> Option<EnvironmentPin> {
            let selector = selector.as_ref()?;
            members
                .iter()
                .find(|member| {
                    member.unit_id == unit_id
                        && member.revision == revision
                        && member.artifact_digest == *digest
                })
                .map(|member| EnvironmentPin {
                    generation: selector.generation,
                    generation_digest: selector.generation_digest.clone(),
                    selector_revision: selector.revision,
                    role: member.role,
                    declarations: member.declarations.clone(),
                })
        };
        let (fact, kind, environment) = match selection {
            PluginLoadSelection::Environment { expected } => {
                let active = selector.as_ref().context(
                    "PLUGIN_LOAD_ENVIRONMENT: this installation has no active environment \
                     generation; prepare and activate one before loading its pin",
                )?;
                if let Some(expected) = &expected {
                    witness(expected)?;
                }
                let member = members
                    .iter()
                    .find(|member| member.unit_id == unit_id)
                    .with_context(|| {
                        format!(
                            "PLUGIN_LOAD_NOT_SELECTED: the active environment generation {} does \
                             not pin {unit_id}",
                            active.generation
                        )
                    })?;
                let fact = self
                    .retained_unit_fact(
                        &member.unit_id,
                        &member.revision,
                        &member.artifact_digest,
                    )?
                    .with_context(|| {
                        format!(
                            "PLUGIN_LOAD_RECORD: {}@{} pinned by generation {} is not retained",
                            member.unit_id, member.revision, active.generation
                        )
                    })?;
                let unit_enabled = self.unit_enabled_state(unit_id)?;
                ensure!(
                    unit_enabled,
                    "PLUGIN_LOAD_DISABLED: {unit_id} is installed but not enabled"
                );
                let environment = pin_of(
                    &member.unit_id,
                    &member.revision,
                    &member.artifact_digest,
                );
                (fact, PluginSelectionKind::Environment, environment)
            }
            PluginLoadSelection::Current { expected } => {
                if let Some(expected) = &expected {
                    witness(expected)?;
                }
                let fact = self.installed_unit_fact(unit_id)?.with_context(|| {
                    format!("PLUGIN_LOAD_RECORD: {unit_id} has no installed version on this node")
                })?;
                ensure!(
                    fact.enabled,
                    "PLUGIN_LOAD_DISABLED: {}@{} is installed but not enabled",
                    fact.unit_id,
                    fact.revision
                );
                if expected.is_some() {
                    let active = selector.as_ref().context(
                        "PLUGIN_LOAD_GENERATION: a verified witness requires an active selector",
                    )?;
                    let member = members
                        .iter()
                        .find(|member| member.unit_id == unit_id)
                        .with_context(|| {
                            format!(
                                "PLUGIN_LOAD_NOT_SELECTED: the observed generation {} does not \
                                 pin {unit_id}",
                                active.generation
                            )
                        })?;
                    ensure!(
                        member.revision == fact.revision
                            && member.artifact_digest == fact.artifact_digest,
                        "PLUGIN_LOAD_GENERATION: the observed generation {} pins {}@{} but the \
                         installation's current version is {}@{}",
                        active.generation,
                        member.unit_id,
                        member.revision,
                        fact.unit_id,
                        fact.revision
                    );
                }
                let environment = pin_of(unit_id, &fact.revision, &fact.artifact_digest);
                (fact, PluginSelectionKind::Current, environment)
            }
            PluginLoadSelection::Exact { identity, expected } => {
                identity
                    .validate()
                    .map_err(|error| anyhow::anyhow!("PLUGIN_LOAD_IDENTITY: {error}"))?;
                ensure!(
                    identity.unit_id == unit_id,
                    "PLUGIN_LOAD_IDENTITY: the exact identity names {} but the request names \
                     {unit_id}",
                    identity.unit_id
                );
                if let Some(expected) = &expected {
                    witness(expected)?;
                }
                let fact = self
                    .retained_unit_fact(
                        &identity.unit_id,
                        &identity.revision,
                        &identity.artifact_digest,
                    )?
                    .with_context(|| {
                        format!(
                            "PLUGIN_LOAD_RECORD: {}@{}#{} is not retained on this node",
                            identity.unit_id, identity.revision, identity.artifact_digest
                        )
                    })?;
                let environment = pin_of(
                    &identity.unit_id,
                    &identity.revision,
                    &identity.artifact_digest,
                );
                (fact, PluginSelectionKind::Exact, environment)
            }
        };
        ensure!(
            fact.kind.is_executable(),
            "PLUGIN_LOAD_KIND: {} is a {} unit, not executable code",
            fact.unit_id,
            fact.kind.as_str()
        );
        let descriptor_digest = Digest::blake3(&serde_json::to_vec(&fact.descriptor)?);
        let declared = external_artifact_of(&fact.descriptor);
        match (&fact.artifact_source, declared) {
            (UnitArtifactSource::Inline, None) => {}
            (UnitArtifactSource::Inline, Some(_)) => bail!(
                "PLUGIN_LOAD_BINDING: {} reports inline bytes but its descriptor declares an \
                 out-of-line artifact",
                fact.unit_id
            ),
            (
                UnitArtifactSource::External {
                    store,
                    locator,
                    bytes,
                },
                Some(declared),
            ) => {
                ensure!(
                    declared.digest == fact.artifact_digest
                        && declared.bytes == *bytes
                        && declared.store == *store
                        && declared.locator == *locator,
                    "PLUGIN_LOAD_BINDING: {} artifact declaration disagrees with the retained \
                     identity",
                    fact.unit_id
                );
                bail!(
                    "PLUGIN_LOAD_ARTIFACT: {} bytes live outside the Node object store (store \
                     {store}); resolve and re-verify them against {} before execution",
                    fact.unit_id,
                    fact.artifact_digest
                );
            }
            (UnitArtifactSource::External { store, .. }, None) => bail!(
                "PLUGIN_LOAD_BINDING: {} reports an out-of-line artifact without a declaration \
                 (store {store})",
                fact.unit_id
            ),
        }
        let bytes: Vec<u8> = self
            .conn
            .query_row(
                "SELECT bytes FROM objects WHERE digest=?1",
                [&fact.artifact_digest.0],
                |row| row.get(0),
            )
            .optional()?
            .with_context(|| {
                format!(
                    "PLUGIN_LOAD_ARTIFACT: {} inline artifact bytes are not retained",
                    fact.unit_id
                )
            })?;
        ensure!(
            bytes.len() as u64 <= MAX_ARTIFACT_BYTES as u64,
            "PLUGIN_LOAD_ARTIFACT: {} exceeds the inline artifact byte bound",
            fact.unit_id
        );
        let content_digest = Digest::blake3(&bytes);
        ensure!(
            content_digest == fact.artifact_digest,
            "PLUGIN_LOAD_HASH: {} retained bytes hash to {content_digest}, not the recorded {}",
            fact.unit_id,
            fact.artifact_digest
        );
        let unit_enabled = self.unit_enabled_state(unit_id)?;
        let receipt = PluginLoadReceipt {
            schema: PLUGIN_LOAD_RECEIPT_SCHEMA,
            unit_id: fact.unit_id,
            kind: fact.kind,
            revision: fact.revision,
            artifact_digest: fact.artifact_digest,
            descriptor_digest,
            content_digest,
            bytes: bytes.len() as u64,
            artifact_source: fact.artifact_source,
            selection: kind,
            environment,
            current: fact.current,
            unit_enabled,
        };
        Ok(InstalledPluginCode {
            receipt,
            descriptor: fact.descriptor,
            bytes: bytes.into(),
        })
    }

    /// Load the exact installed unit one prepared generation binding names.
    ///
    /// A sealed request already carries the unit/revision/artifact identity it
    /// was admitted against (see
    /// [`shenko_types::generation::SealedGenerationBinding`]), so a runtime
    /// generation owner does not have to translate it back into a name lookup:
    /// the binding is the selector. A compiled role names no installed unit
    /// and is refused here; it has no installed code to load.
    pub fn load_sealed_generation_unit(
        &self,
        binding: &shenko_types::generation::SealedGenerationBinding,
        expected: Option<PluginGenerationWitness>,
    ) -> Result<InstalledPluginCode> {
        binding
            .validate()
            .map_err(|error| anyhow::anyhow!("PLUGIN_LOAD_IDENTITY: {error}"))?;
        let identity = binding.unit.clone().context(
            "PLUGIN_LOAD_IDENTITY: this prepared role is compiled into the running binary and \
             names no installed code unit",
        )?;
        // The unit id is read before the identity moves into the selection.
        let unit_id = identity.unit_id.clone();
        self.load_installed_plugin_code(
            &unit_id,
            PluginLoadSelection::Exact { identity, expected },
        )
    }

    /// Whether the installation currently has this unit enabled. Enablement is
    /// a fact of the unit, not of the version a generation happens to pin, so
    /// it is read from `unit_current` by unit id.
    fn unit_enabled_state(&self, unit_id: &str) -> Result<bool> {
        let enabled: Option<i64> = self
            .conn
            .query_row(
                "SELECT enabled FROM unit_current WHERE unit_id=?1",
                [unit_id],
                |row| row.get(0),
            )
            .optional()?;
        match enabled {
            None => Ok(false),
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            Some(_) => bail!("PLUGIN_LOAD_RECORD: {unit_id} has an invalid enable projection"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::test_support::{export, package, scratch};
    use crate::units::{
        EnvironmentRoot, ExternalArtifact, UnitPackage, UnitRequest,
    };
    use serde_json::{Value, json};
    use std::path::Path;

    fn code(unit_id: &str, revision: &str, bytes: &[u8]) -> UnitPackage {
        let mut unit = package(unit_id, revision, bytes, Vec::new());
        unit.unit.kind = UnitKind::Code;
        unit.unit.config = json!({"mode": "test"});
        unit
    }

    fn install(node: &mut Node, root: &Path, packages: Vec<UnitPackage>, enable: bool) {
        node.handle_unit_request(UnitRequest::Import {
            export: export(packages),
            package_root: root.to_string_lossy().into_owned(),
            enable: Some(enable),
        })
        .unwrap();
    }

    fn activate(node: &mut Node, unit_id: &str) -> u64 {
        let prepared = node
            .handle_unit_request(UnitRequest::PrepareEnvironment {
                roots: vec![EnvironmentRoot {
                    unit_id: unit_id.into(),
                    revision: None,
                    artifact_digest: None,
                }],
            })
            .unwrap();
        let generation = prepared["generation"]["generation"].as_u64().unwrap();
        let revision = node
            .environment_selector()
            .unwrap()
            .map(|selector| selector.revision)
            .unwrap_or(0);
        node.handle_unit_request(UnitRequest::ActivateEnvironment {
            generation,
            expected_revision: revision,
        })
        .unwrap();
        generation
    }

    fn selector_witness(node: &Node) -> PluginGenerationWitness {
        let selector = node.environment_selector().unwrap().unwrap();
        PluginGenerationWitness::of_selector(&selector)
    }

    fn error(result: Result<InstalledPluginCode>) -> String {
        format!("{:#}", result.unwrap_err())
    }

    #[test]
    fn current_load_verifies_bytes_and_records_the_authority_chain() {
        let root = scratch("plugin-load-current");
        let packages = scratch("plugin-load-current-packages");
        let mut node = Node::open(&root).unwrap();
        install(
            &mut node,
            &packages,
            vec![code("plugin.alpha", "1.0.0", b"plugin-bytes")],
            true,
        );
        let loaded = node
            .load_installed_plugin_code(
                "plugin.alpha",
                PluginLoadSelection::Current { expected: None },
            )
            .unwrap();
        let digest = Digest::blake3(b"plugin-bytes");
        assert_eq!(loaded.bytes(), b"plugin-bytes".as_slice());
        assert_eq!(loaded.descriptor().unit_id, "plugin.alpha");
        let receipt = loaded.receipt();
        assert_eq!(receipt.schema, PLUGIN_LOAD_RECEIPT_SCHEMA);
        assert_eq!(receipt.selection, PluginSelectionKind::Current);
        assert_eq!(receipt.artifact_digest, digest);
        assert_eq!(receipt.content_digest, digest);
        assert_eq!(receipt.bytes, 12);
        assert_eq!(receipt.environment, None);
        assert!(receipt.current);
        assert!(receipt.unit_enabled);
        assert_eq!(
            receipt.descriptor_digest,
            Digest::blake3(&serde_json::to_vec(loaded.descriptor()).unwrap())
        );
        // The receipt is reproducible from the same installation facts.
        let again = node
            .load_installed_plugin_code(
                "plugin.alpha",
                PluginLoadSelection::Current { expected: None },
            )
            .unwrap();
        assert_eq!(receipt.digest().unwrap(), again.receipt().digest().unwrap());
    }

    #[test]
    fn load_refuses_absent_disabled_and_non_code_records() {
        let root = scratch("plugin-load-absences");
        let packages = scratch("plugin-load-absences-packages");
        let mut node = Node::open(&root).unwrap();
        assert!(error(node.load_installed_plugin_code(
            "plugin.missing",
            PluginLoadSelection::Current { expected: None },
        ))
        .contains("PLUGIN_LOAD_RECORD"));

        // A Skill unit is content, not executable plugin code.
        let mut content = package("plugin.content", "1.0.0", b"content", Vec::new());
        content.unit.kind = UnitKind::Skill;
        install(&mut node, &packages, vec![content], true);
        assert!(error(node.load_installed_plugin_code(
            "plugin.content",
            PluginLoadSelection::Current { expected: None },
        ))
        .contains("PLUGIN_LOAD_KIND"));

        install(
            &mut node,
            &packages,
            vec![code("plugin.disabled", "1.0.0", b"disabled-bytes")],
            false,
        );
        assert!(error(node.load_installed_plugin_code(
            "plugin.disabled",
            PluginLoadSelection::Current { expected: None },
        ))
        .contains("PLUGIN_LOAD_DISABLED"));
    }

    #[test]
    fn environment_load_follows_the_pin_instead_of_the_current_row() {
        let root = scratch("plugin-load-generation");
        let packages = scratch("plugin-load-generation-packages");
        let mut node = Node::open(&root).unwrap();
        install(
            &mut node,
            &packages,
            vec![code("plugin.pinned", "1.0.0", b"pinned-v1-bytes")],
            true,
        );
        let generation = activate(&mut node, "plugin.pinned");
        let witness = selector_witness(&node);

        let pinned = node
            .load_installed_plugin_code(
                "plugin.pinned",
                PluginLoadSelection::Environment {
                    expected: Some(witness.clone()),
                },
            )
            .unwrap();
        assert_eq!(pinned.bytes(), b"pinned-v1-bytes".as_slice());
        let pin = pinned.receipt().environment.clone().unwrap();
        assert_eq!(pin.generation, generation);
        assert_eq!(pin.selector_revision, witness.selector_revision);
        assert_eq!(pin.role.as_str(), "root");
        assert_eq!(pinned.receipt().selection, PluginSelectionKind::Environment);
        assert!(pinned.receipt().current);

        // A later import of another revision moves `unit_current` but never
        // rewrites the generation: a generation-bound load keeps the pin.
        install(
            &mut node,
            &packages,
            vec![code("plugin.pinned", "2.0.0", b"pinned-v2-bytes")],
            true,
        );
        let still_pinned = node
            .load_installed_plugin_code(
                "plugin.pinned",
                PluginLoadSelection::Environment {
                    expected: Some(witness),
                },
            )
            .unwrap();
        assert_eq!(still_pinned.bytes(), b"pinned-v1-bytes".as_slice());
        assert_eq!(still_pinned.receipt().revision, "1.0.0");
        assert!(!still_pinned.receipt().current);
        assert_eq!(
            still_pinned.receipt().environment.clone().unwrap().generation,
            generation
        );

        let current = node
            .load_installed_plugin_code(
                "plugin.pinned",
                PluginLoadSelection::Current { expected: None },
            )
            .unwrap();
        assert_eq!(current.bytes(), b"pinned-v2-bytes".as_slice());
        assert_eq!(current.receipt().selection, PluginSelectionKind::Current);
        assert_eq!(current.receipt().environment, None);
    }

    #[test]
    fn load_refuses_a_stale_witness_and_an_unpinned_unit() {
        let root = scratch("plugin-load-witness");
        let packages = scratch("plugin-load-witness-packages");
        let mut node = Node::open(&root).unwrap();
        install(
            &mut node,
            &packages,
            vec![
                code("plugin.one", "1.0.0", b"one-bytes"),
                code("plugin.two", "1.0.0", b"two-bytes"),
            ],
            true,
        );
        // No generation yet: a conditional load is refused rather than
        // accepted without the generation the caller observed.
        let unpinned_witness = PluginGenerationWitness {
            generation: 0,
            digest: Digest::blake3(b"none"),
            selector_revision: 0,
        };
        assert!(error(node.load_installed_plugin_code(
            "plugin.one",
            PluginLoadSelection::Current {
                expected: Some(unpinned_witness.clone()),
            },
        ))
        .contains("PLUGIN_LOAD_ENVIRONMENT"));
        assert!(error(node.load_installed_plugin_code(
            "plugin.one",
            PluginLoadSelection::Environment { expected: None },
        ))
        .contains("PLUGIN_LOAD_ENVIRONMENT"));

        activate(&mut node, "plugin.one");
        // The import enables the export's root only; the other unit stays
        // installed and disabled until it is enabled on purpose. Enabling it
        // here makes the next refusal the pin, not the enable state.
        node.handle_unit_request(UnitRequest::SetEnabled {
            unit_id: "plugin.two".into(),
            enabled: true,
        })
        .unwrap();
        let witness = selector_witness(&node);
        let stale = PluginGenerationWitness {
            selector_revision: witness.selector_revision + 1,
            ..witness.clone()
        };
        assert!(error(node.load_installed_plugin_code(
            "plugin.one",
            PluginLoadSelection::Current {
                expected: Some(stale.clone()),
            },
        ))
        .contains("PLUGIN_LOAD_GENERATION"));
        // plugin.two is installed but not a member of the active generation.
        assert!(error(node.load_installed_plugin_code(
            "plugin.two",
            PluginLoadSelection::Environment {
                expected: Some(witness.clone()),
            },
        ))
        .contains("PLUGIN_LOAD_NOT_SELECTED"));
        // A conditional current load must agree with the generation's pin.
        let unpinned_current = error(node.load_installed_plugin_code(
            "plugin.two",
            PluginLoadSelection::Current {
                expected: Some(witness.clone()),
            },
        ));
        assert!(
            unpinned_current.contains("PLUGIN_LOAD_NOT_SELECTED"),
            "{unpinned_current}"
        );
        // An exact frozen identity still loads outside the generation, but its
        // receipt says it is not the generation's pin.
        let outside = node
            .load_installed_plugin_code(
                "plugin.two",
                PluginLoadSelection::Exact {
                    identity: SealedUnitIdentity {
                        unit_id: "plugin.two".into(),
                        revision: "1.0.0".into(),
                        artifact_digest: Digest::blake3(b"two-bytes"),
                    },
                    expected: Some(witness),
                },
            )
            .unwrap();
        assert_eq!(outside.receipt().selection, PluginSelectionKind::Exact);
        assert_eq!(outside.receipt().environment, None);
    }

    #[test]
    fn exact_load_reaches_a_retained_version_without_claiming_selection() {
        let root = scratch("plugin-load-exact");
        let packages = scratch("plugin-load-exact-packages");
        let mut node = Node::open(&root).unwrap();
        install(
            &mut node,
            &packages,
            vec![code("plugin.frozen", "1.0.0", b"frozen-bytes")],
            true,
        );
        let generation = activate(&mut node, "plugin.frozen");
        install(
            &mut node,
            &packages,
            vec![code("plugin.frozen", "2.0.0", b"newer-bytes")],
            true,
        );
        let identity = SealedUnitIdentity {
            unit_id: "plugin.frozen".into(),
            revision: "1.0.0".into(),
            artifact_digest: Digest::blake3(b"frozen-bytes"),
        };
        let loaded = node
            .load_installed_plugin_code(
                "plugin.frozen",
                PluginLoadSelection::Exact {
                    identity,
                    expected: Some(selector_witness(&node)),
                },
            )
            .unwrap();
        assert_eq!(loaded.bytes(), b"frozen-bytes".as_slice());
        assert_eq!(loaded.receipt().selection, PluginSelectionKind::Exact);
        assert!(!loaded.receipt().current);
        assert_eq!(
            loaded.receipt().environment.clone().unwrap().generation,
            generation
        );
        assert!(loaded.receipt().unit_enabled);

        // A malformed or mismatched exact identity never reaches a load.
        assert!(error(node.load_installed_plugin_code(
            "plugin.frozen",
            PluginLoadSelection::Exact {
                identity: SealedUnitIdentity {
                    unit_id: "".into(),
                    revision: "1.0.0".into(),
                    artifact_digest: Digest::blake3(b"frozen-bytes"),
                },
                expected: None,
            },
        ))
        .contains("PLUGIN_LOAD_IDENTITY"));
        assert!(error(node.load_installed_plugin_code(
            "plugin.frozen",
            PluginLoadSelection::Exact {
                identity: SealedUnitIdentity {
                    unit_id: "plugin.other".into(),
                    revision: "1.0.0".into(),
                    artifact_digest: Digest::blake3(b"frozen-bytes"),
                },
                expected: None,
            },
        ))
        .contains("PLUGIN_LOAD_IDENTITY"));
        assert!(error(node.load_installed_plugin_code(
            "plugin.frozen",
            PluginLoadSelection::Exact {
                identity: SealedUnitIdentity {
                    unit_id: "plugin.frozen".into(),
                    revision: "9.9.9".into(),
                    artifact_digest: Digest::blake3(b"frozen-bytes"),
                },
                expected: None,
            },
        ))
        .contains("PLUGIN_LOAD_RECORD"));
    }

    #[test]
    fn sealed_generation_binding_selects_its_exact_installed_unit() {
        use shenko_types::generation::{PreparedRole, SealedGenerationBinding};
        use shenko_types::object::{ArtifactRef, SchemaRef, VersionedRef};

        let root = scratch("plugin-load-sealed");
        let packages = scratch("plugin-load-sealed-packages");
        let mut node = Node::open(&root).unwrap();
        install(
            &mut node,
            &packages,
            vec![code("plugin.sealed", "1.0.0", b"sealed-bytes")],
            true,
        );
        let implementation = VersionedRef {
            artifact: ArtifactRef {
                digest: Digest::blake3(b"sealed-bytes"),
                locator: "builtin:plugin.sealed".into(),
            },
            symbol: "plugin.sealed".into(),
            schema: SchemaRef {
                name: "plugin.sealed".into(),
                major: 1,
                digest: Digest::blake3(b"plugin.sealed/v1"),
            },
        };
        let binding = SealedGenerationBinding::prepared_unit(
            PreparedRole::Provider,
            SealedUnitIdentity {
                unit_id: "plugin.sealed".into(),
                revision: "1.0.0".into(),
                artifact_digest: Digest::blake3(b"sealed-bytes"),
            },
            "plugin.sealed/export",
            implementation.clone(),
            Digest::blake3(b"manifest"),
            Digest::blake3(b"descriptor"),
            Digest::blake3(b"closure"),
        );
        let loaded = node.load_sealed_generation_unit(&binding, None).unwrap();
        assert_eq!(loaded.bytes(), b"sealed-bytes".as_slice());
        assert_eq!(loaded.receipt().selection, PluginSelectionKind::Exact);
        assert_eq!(loaded.receipt().unit_id, "plugin.sealed");

        // A compiled role names no installed unit; it has no code to load.
        let compiled = SealedGenerationBinding::compiled(
            PreparedRole::Provider,
            implementation,
            Digest::blake3(b"descriptor"),
            Digest::blake3(b"closure"),
        );
        assert!(error(node.load_sealed_generation_unit(&compiled, None))
            .contains("PLUGIN_LOAD_IDENTITY"));
    }

    #[test]
    fn load_refuses_bytes_that_do_not_hash_to_the_retained_digest() {
        let root = scratch("plugin-load-tamper");
        let packages = scratch("plugin-load-tamper-packages");
        let mut node = Node::open(&root).unwrap();
        install(
            &mut node,
            &packages,
            vec![code("plugin.tampered", "1.0.0", b"original-bytes")],
            true,
        );
        let digest = Digest::blake3(b"original-bytes");
        node.conn
            .execute(
                "UPDATE objects SET bytes=?1 WHERE digest=?2",
                rusqlite::params![b"tampered".to_vec(), digest.0],
            )
            .unwrap();
        assert!(error(node.load_installed_plugin_code(
            "plugin.tampered",
            PluginLoadSelection::Current { expected: None },
        ))
        .contains("PLUGIN_LOAD_HASH"));
    }

    #[test]
    fn load_refuses_out_of_line_bytes_and_an_inconsistent_binding() {
        let root = scratch("plugin-load-external");
        let packages = scratch("plugin-load-external-packages");
        let mut node = Node::open(&root).unwrap();
        let declared = Digest::blake3(b"external-declared");
        node.handle_unit_request(UnitRequest::ImportExternal {
            unit: UnitDescriptor {
                unit_id: "plugin.external".into(),
                kind: UnitKind::Code,
                revision: "1.0.0".into(),
                artifact_digest: declared.clone(),
                provides: Vec::new(),
                dependencies: Vec::new(),
                config: json!({}),
                applicability: None,
            },
            artifact: ExternalArtifact {
                digest: declared,
                bytes: 4096,
                store: "cas".into(),
                locator: "cas/plugin.external".into(),
            },
            package_root: packages.to_string_lossy().into_owned(),
            enable: Some(true),
        })
        .unwrap();
        assert!(error(node.load_installed_plugin_code(
            "plugin.external",
            PluginLoadSelection::Current { expected: None },
        ))
        .contains("PLUGIN_LOAD_ARTIFACT"));

        // A record whose descriptor declares a different out-of-line identity
        // than the inventory key is refused before any bytes are considered.
        install(
            &mut node,
            &packages,
            vec![code("plugin.binding", "1.0.0", b"binding-bytes")],
            true,
        );
        let digest = Digest::blake3(b"binding-bytes");
        let data: String = node
            .conn
            .query_row(
                "SELECT data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND \
                 artifact_digest=?3",
                rusqlite::params!["plugin.binding", "1.0.0", digest.0],
                |row| row.get(0),
            )
            .unwrap();
        let mut stored: Value = serde_json::from_str(&data).unwrap();
        stored["unit"]["config"]["external_artifact"] = json!({
            "digest": Digest::blake3(b"other-bytes").0,
            "bytes": 1,
            "store": "cas",
            "locator": "cas/other",
        });
        node.conn
            .execute(
                "UPDATE unit_versions SET data=?1 WHERE unit_id=?2 AND revision=?3 AND \
                 artifact_digest=?4",
                rusqlite::params![
                    serde_json::to_string(&stored).unwrap(),
                    "plugin.binding",
                    "1.0.0",
                    digest.0
                ],
            )
            .unwrap();
        assert!(error(node.load_installed_plugin_code(
            "plugin.binding",
            PluginLoadSelection::Current { expected: None },
        ))
        .contains("PLUGIN_LOAD_BINDING"));
    }
}
