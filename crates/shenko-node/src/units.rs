//! Installation units: cross-node package transfer, per-node installation
//! facts, and real component trials.
//!
//! The unit descriptor shapes the unit itself (identity, kind, revision, artifact,
//! dependencies, config, applicability). The rules this module keeps:
//!
//! * Copying a package never makes a unit ready *here*. Only a locally
//!   executed trial of the exact artifact digest in this environment can.
//! * Declared OS/arch/runtime labels are advisory. A foreign, incomplete or
//!   unknown label never blocks a trial; only a real failure does.
//! * A trial attempt is durable before the child starts. A crash leaves the
//!   attempt unresolved (unknown) and it is never replayed automatically.
//! * A partial import or a partial materialization never reports ready.
//! * Local bindings stay local. This module never reads an environment
//!   value, a secret file or a provider grant into a package, and it exports
//!   no such material. The package's own declared bytes and configuration are
//!   copied exactly as they are, so a package that declares a secret carries
//!   that secret with it.
//!
//! Responsibility owners are private submodules with explicit imports: `codec`
//! and `capabilities` own portable package data; `schema` owns storage admission
//! and old migrations; `transfer`, `installation` and `resolution` own imports,
//! inventory and dependency choices; `environment` and `membership` own their
//! locked projections; `materialization` owns filesystem copies; `process`,
//! `trials` and `adoption` own execution lifetimes and evidence; `removal` owns
//! withdrawal facts. This module retains the request and public contract.
//!
//! All owners use the same private `Node::conn` and existing inline `objects`
//! table. Splitting responsibilities does not create another storage authority.

mod adoption;
mod capabilities;
mod codec;
mod environment;
mod installation;
mod materialization;
mod membership;
mod process;
mod removal;
mod resolution;
mod schema;
mod transfer;
mod trials;

#[cfg(test)]
mod test_support;

pub(crate) use schema::{UNIT_REMOVALS_TABLE, UNIT_TABLES_V3, migrate_v3_edges};
pub use adoption::{
    AdoptionDecision, AdoptionEvidence, AdoptionEvidenceRejected, AdoptionEvidenceRejection,
    AdoptionMethod, ContentAdoption, ContentAdoptionEvidence, MacroContentAdoption,
    UnitAdoptionProjection,
};
pub use capabilities::{
    AtomicCapability, CAPABILITY_CONTRACT_PREFIX, CapabilityContract, CapabilityExclusivity,
    FileRequirement, HostRequirement, MAX_ATOMIC_CAPABILITIES, MAX_CAPABILITY_NAME_BYTES,
    MAX_CONTRACT_BYTES, MAX_DECLARED_ENVIRONMENT, MAX_DECLARED_HOST,
    MAX_DECLARED_NETWORK_TARGETS, MAX_DECLARED_PATHS, NetworkOperation, NetworkTarget,
    PackageCapabilities, PackageDeclaration, atomic_conflict, declaration_document,
    declared_package_capabilities,
};
pub use codec::{
    Applicability, ExternalArtifact, MAX_ARTIFACT_BYTES, MAX_EDGES, MAX_EXTERNAL_ARTIFACT_BYTES,
    MAX_PACKAGES, MAX_PROVIDES, MAX_TEXT_BYTES, UNIT_SCHEMA, UnitArtifactSource, UnitDescriptor,
    UnitExport, UnitKind, UnitPackage, b64_decode, b64_encode,
};
pub use environment::{
    ENVIRONMENT_CLOSURE_VERSION, EnvironmentGeneration, EnvironmentMember, EnvironmentPin,
    EnvironmentRole, EnvironmentRoot, EnvironmentSelector, MAX_ENVIRONMENT_MEMBERS,
    MAX_ENVIRONMENT_ROOTS, UNIT_ENVIRONMENT_SCHEMA,
};
pub use installation::{InstalledUnitFact, SupersededVersion, ToolUnitFact, UnitContent};
pub use materialization::{MaterializationFact, RetainedPath};
pub use membership::{
    CapabilityProviderDefault, DeclaredBy, DeclaredTargetAccess, DistributionBody,
    MAX_MACRO_DEPTH, MAX_MACRO_MEMBERS, MacroMemberSpec, MacroSlot, MemberAdoption,
    MemberSelection, MembershipResolution, MembershipStatus, MetapackageBody, PreinstalledMacro,
    TargetAccessExpectation, declared_distribution, declared_metapackage,
};
pub use process::{
    CommandTrialProvenance, DEFAULT_TRIAL_TIMEOUT_MS, MAX_ARGV, MAX_ARG_BYTES, MAX_OUTPUT_BYTES,
    MAX_TRIAL_TIMEOUT_MS, ProgramProbe, TrialEnvironment, TrialEvidence,
};
pub use removal::{RemovedVersionFact, UnitActivation};
pub use resolution::{
    DependencySelector, DependencySpec, DependencyStrength, EdgeStatus, ProviderSelection,
    ReadinessState, ResolvedDependency,
};
pub use schema::UNIT_TABLES;
pub use trials::{
    AttemptMeasurement, GenericTrialProvenance, TrialAttemptFacts, TrialOutcome,
    TrialProvenance,
};

use anyhow::Result;
use crate::Node;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_types::object::Digest;
use self::schema::ensure_unit_schema;

// ---------------------------------------------------------------- request ---

/// Request surface of the installation-unit handler.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum UnitRequest {
    /// Verify and store a package closure, then materialize its bytes.
    Import {
        export: UnitExport,
        package_root: String,
        enable: Option<bool>,
    },
    /// Retry materialization of an installed unit into a package root.
    Materialize {
        unit_id: String,
        package_root: String,
    },
    /// Make a new resolution decision for one retained version without
    /// changing which version is current or rerunning any component.
    ResolveVersion {
        unit_id: String,
        revision: String,
        artifact_digest: Digest,
    },
    /// Local facts of one installed unit.
    Status { unit_id: String },
    /// Local facts of every installed unit.
    List {},
    /// Export a unit and the dependency closure available on this node.
    Export { unit_id: String },
    /// Enable or disable a unit on this node.
    SetEnabled { unit_id: String, enabled: bool },
    /// Run a bounded, caller-selected command as the real trial of a unit.
    Trial {
        unit_id: String,
        argv: Vec<String>,
        cwd: Option<String>,
        timeout_ms: Option<u64>,
        risk: String,
        benefit: String,
        consequence: String,
    },
    /// Append-only trial evidence, optionally of one unit.
    Evidence {
        unit_id: Option<String>,
        after: String,
        limit: u32,
    },
    /// Register a unit version whose artifact bytes live outside this
    /// installation's inline object store.
    ///
    /// The node records the declared identity — digest, size and the
    /// installation-local locator — and never copies the bytes. The
    /// installer verifies the bytes before registering them and the
    /// executor re-verifies them at run time, so an artifact that does not
    /// match its descriptor still never executes.
    ImportExternal {
        unit: UnitDescriptor,
        artifact: ExternalArtifact,
        package_root: String,
        enable: Option<bool>,
    },
    /// Read the exact content of one installed unit version.
    ///
    /// The revision is mandatory: a unit keeps several retained versions and
    /// the caller has to name the one it wants.
    Read { unit_id: String, revision: String },
    /// Record how one unit version was judged, and on which evidence.
    ///
    /// The decision is a fact about this installation, never a conclusion
    /// drawn from the unit's text. The node stores the decision, the method
    /// and the evidence the caller actually observed.
    RecordAdoption {
        unit_id: String,
        revision: String,
        artifact_digest: Digest,
        decision: AdoptionDecision,
        method: AdoptionMethod,
        /// The evidence the judgement rests on. The node verifies every item
        /// itself: a trial attempt must exist in this node's own trial store
        /// for this exact unit version, and a stored observation must exist
        /// in this node's own object store and be bound to this version.
        /// An observation is never empirical acceptance.
        evidence: Vec<AdoptionEvidence>,
        /// How confident the subject is in this judgement, when it states
        /// one. Absent means the subject has not judged yet: the attitude
        /// records that as unknown and never fills a default number.
        confidence: Option<f64>,
        notes: Option<String>,
        /// Enable the unit in the same transaction. A rejected or retired
        /// decision never enables it. Enabling is an installation fact and
        /// never requires evidence; empirical acceptance is a separate fact
        /// that the recorded evidence decides.
        enable: Option<bool>,
    },
    /// Open a durable trial attempt for one unit version.
    ///
    /// The attempt exists before anything runs, so a crash leaves an
    /// unresolved attempt instead of a missing fact. The plan records what
    /// the trial is bound to: the exact artifact, the scenarios, the
    /// candidate and control configuration and the model binding.
    BeginTrial {
        unit_id: String,
        revision: String,
        artifact_digest: Digest,
        /// What kind of trial this is, e.g. `sandbox_comparison`.
        kind: String,
        /// The bound facts of the trial. The node digests it and keeps the
        /// digest with the attempt.
        plan: Value,
    },
    /// Close a trial attempt with the receipts the run produced.
    ///
    /// Every receipt is an independent terminal fact of one condition (one
    /// committed decision, one judge verdict). The node stores them with
    /// the attempt; it never derives them from a summary.
    FinishTrial {
        attempt_id: String,
        outcome: TrialOutcome,
        receipts: Vec<Value>,
    },
    /// Register an observation about one unit version.
    ///
    /// An observation is a recorded fact (a measurement, a report, a note).
    /// It is stored with its bytes so the node can re-verify them, and it
    /// is bound to this exact unit version. An observation never becomes
    /// empirical acceptance by itself.
    RegisterObservation {
        unit_id: String,
        revision: String,
        artifact_digest: Digest,
        kind: String,
        bytes: Vec<u8>,
    },
    /// The observations registered for one unit version.
    Observations { unit_id: String },
    /// The retained versions of one unit and the supersession relations
    /// between them.
    Lineage { unit_id: String },
    /// Withdraw one installed version from this node.
    ///
    /// The version leaves discovery: it stops being current, its
    /// materialized package directory is retired, and its dependency rows are
    /// archived. Its artifact bytes, trial attempts, observations and adoption
    /// decision stay as facts, so a receipt that named this artifact remains
    /// readable. A version another retained unit still depends on, or one
    /// owned by a running trial, is refused instead of silently breaking the
    /// dependent closure.
    Uninstall {
        unit_id: String,
        revision: String,
        /// Mandatory when the revision retains several artifact digests.
        artifact_digest: Option<Digest>,
        /// Why this version is withdrawn. Recorded with the removal fact.
        reason: String,
        /// A different retained version to offer as current in the same
        /// transaction. Never chosen implicitly.
        activate: Option<UnitActivation>,
    },
    /// Resolve the transitive required closure of these roots against the
    /// versions retained here and record it as a locked generation.
    ///
    /// Nothing becomes current: preparation is a candidate. A closure that
    /// cannot be satisfied from retained, resolved edges is refused with every
    /// problem listed, so an operator never receives a half-resolved
    /// environment.
    PrepareEnvironment { roots: Vec<EnvironmentRoot> },
    /// Atomically select a prepared generation as this installation's current
    /// environment. `expected_revision` is the selector revision the caller
    /// read; the write is a compare-and-swap, so a concurrent activation
    /// fails loudly instead of being overwritten.
    ActivateEnvironment { generation: u64, expected_revision: u64 },
    /// Select the previous generation of the current environment. The same
    /// compare-and-swap as activation: the caller names the revision it read.
    RollbackEnvironment { expected_revision: u64 },
    /// The current environment selector, the recorded generations and the
    /// append-only activation log. Pure query: an installation that never
    /// activated a generation reports `pinned: false`.
    Environment { limit: u32 },
    /// Walk a metapackage's member list, resolve every member against
    /// installed/retained units, check adoption via the content/attitude
    /// ledger, and return the membership standing of each. Pure read.
    ///
    /// The metapackage must be installed on this node; membership never
    /// crosses nodes. A content member that lacks an authenticated adoption
    /// attitude is reported as unadopted, not omitted.
    QueryMetapackage { unit_id: String },
    /// Walk a distribution's preinstalled macros, target-access expectations,
    /// and capability-provider defaults. Pure read: declarations are facts
    /// about the distribution version, not grants of local readiness.
    QueryDistribution { unit_id: String },
    /// Resolve one named capability-contract slot across the installed
    /// providers on this node. Returns every admitted provider, the
    /// coexistence decision and the selected index (if any). Pure read.
    QuerySlot {
        macro_id: String,
        contract: String,
    },
}

impl Node {
    /// Handle one installation-unit request.
    ///
    /// This is the only entry point for package transfer, per-node
    /// installation state and component trials.
    pub fn handle_unit_request(&mut self, request: UnitRequest) -> Result<Value> {
        let result = self.handle_unit_inner(request);
        if let Err(e) = &result {
            if e.is::<rusqlite::Error>() || e.is::<serde_json::Error>() || e.is::<std::io::Error>()
            {
                self.failed = true;
            }
        }
        result
    }

    fn handle_unit_inner(&mut self, request: UnitRequest) -> Result<Value> {
        ensure_unit_schema(&self.conn)?;
        // A crash between persisting an attempt and recording its result
        // leaves an unresolved attempt. It becomes unknown here and is never
        // replayed automatically.
        let reconciled = self.retire_unknown_trials()?;
        let mut result = match request {
            UnitRequest::Import {
                export,
                package_root,
                enable,
            } => self.unit_import(export, &package_root, enable)?,
            UnitRequest::Materialize {
                unit_id,
                package_root,
            } => self.unit_materialize(&unit_id, &package_root)?,
            UnitRequest::ResolveVersion { unit_id, revision, artifact_digest } => {
                self.unit_resolve_version(&unit_id, &revision, &artifact_digest)?
            }
            UnitRequest::Status { unit_id } => self.unit_status(&unit_id)?,
            UnitRequest::List {} => self.unit_list()?,
            UnitRequest::Export { unit_id } => self.unit_export(&unit_id)?,
            UnitRequest::SetEnabled { unit_id, enabled } => {
                self.unit_set_enabled(&unit_id, enabled)?
            }
            UnitRequest::Trial {
                unit_id,
                argv,
                cwd,
                timeout_ms,
                risk,
                benefit,
                consequence,
            } => self.unit_trial(
                &unit_id,
                &argv,
                cwd.as_deref(),
                timeout_ms,
                &risk,
                &benefit,
                &consequence,
            )?,
            UnitRequest::Evidence {
                unit_id,
                after,
                limit,
            } => self.unit_evidence(unit_id.as_deref(), &after, limit)?,
            UnitRequest::ImportExternal {
                unit,
                artifact,
                package_root,
                enable,
            } => self.unit_import_external(unit, artifact, &package_root, enable)?,
            UnitRequest::Read { unit_id, revision } => self.unit_read(&unit_id, &revision)?,
            UnitRequest::RecordAdoption {
                unit_id,
                revision,
                artifact_digest,
                decision,
                method,
                evidence,
                confidence,
                notes,
                enable,
            } => self.unit_record_adoption(
                &unit_id,
                &revision,
                &artifact_digest,
                decision,
                method,
                &evidence,
                confidence,
                notes.as_deref(),
                enable,
            )?,
            UnitRequest::BeginTrial {
                unit_id,
                revision,
                artifact_digest,
                kind,
                plan,
            } => self.unit_begin_trial(&unit_id, &revision, &artifact_digest, &kind, &plan)?,
            UnitRequest::FinishTrial {
                attempt_id,
                outcome,
                receipts,
            } => self.unit_finish_trial(&attempt_id, outcome, &receipts)?,
            UnitRequest::RegisterObservation {
                unit_id,
                revision,
                artifact_digest,
                kind,
                bytes,
            } => self.unit_register_observation(&unit_id, &revision, &artifact_digest, &kind, &bytes)?,
            UnitRequest::Observations { unit_id } => self.unit_observations(&unit_id)?,
            UnitRequest::Lineage { unit_id } => self.unit_lineage(&unit_id)?,
            UnitRequest::Uninstall {
                unit_id,
                revision,
                artifact_digest,
                reason,
                activate,
            } => self.unit_uninstall(
                &unit_id,
                &revision,
                artifact_digest.as_ref(),
                &reason,
                activate.as_ref(),
            )?,
            UnitRequest::PrepareEnvironment { roots } => self.prepare_environment(&roots)?,
            UnitRequest::ActivateEnvironment {
                generation,
                expected_revision,
            } => self.activate_environment(generation, expected_revision)?,
            UnitRequest::RollbackEnvironment { expected_revision } => {
                self.rollback_environment(expected_revision)?
            }
            UnitRequest::Environment { limit } => self.environment_report(limit)?,
            UnitRequest::QueryMetapackage { unit_id } => self.query_metapackage(&unit_id)?,
            UnitRequest::QueryDistribution { unit_id } => self.query_distribution(&unit_id)?,
            UnitRequest::QuerySlot { macro_id, contract } => {
                self.query_slot(&macro_id, &contract)?
            }
        };
        if let Some(object) = result.as_object_mut() {
            object.insert("schema".into(), json!(UNIT_SCHEMA));
            if reconciled > 0 {
                object.insert("reconciled_attempts".into(), json!(reconciled));
            }
        }
        Ok(result)
    }
}
