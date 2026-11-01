//! Activation record, selector and route authority — SPEC §41.12 (X05/X17).
//!
//! These are the cross-module protocol types of the software-generation
//! hand-over. They describe *what was decided and where it is written*, never
//! that a hand-over happened: the durable state lives in the store's
//! `activation_records` / `activation_selectors` rows and the selector CAS is
//! the single linearization point of an activation.

use serde::{Deserialize, Serialize};

use crate::commit::CommitRef;
use crate::ids::{EntityId, RuntimeGeneration};
use crate::object::{Digest, ObjectRef, SchemaRef, VersionRef};
use crate::subject::GenesisRef;

/// Phase of one activation — SPEC §41.12 X05.
///
/// The phase describes this version's hand-over protocol, not a workflow every
/// configuration change or thought has to pass through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationPhase {
    Prepared,
    Copying,
    CatchUp,
    Smoke,
    Handoff,
    Activated,
    Observing,
    Retired,
    Abandoned,
}

impl ActivationPhase {
    pub fn name(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Copying => "copying",
            Self::CatchUp => "catch_up",
            Self::Smoke => "smoke",
            Self::Handoff => "handoff",
            Self::Activated => "activated",
            Self::Observing => "observing",
            Self::Retired => "retired",
            Self::Abandoned => "abandoned",
        }
    }

    /// Terminal phases end this activation record. A rollback is *not* a
    /// phase: it is a new activation record from the current generation back
    /// to the previous one, so the history stays append-only.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Retired | Self::Abandoned)
    }

    /// The forward phase sequence of §41.12 X16/X17.
    pub fn can_advance_to(self, next: ActivationPhase) -> bool {
        use ActivationPhase::*;
        match (self, next) {
            (Prepared, Copying)
            | (Copying, CatchUp)
            | (CatchUp, Smoke)
            | (Smoke, Handoff)
            | (Handoff, Activated)
            | (Activated, Observing)
            | (Observing, Retired) => true,
            // Any live phase may be abandoned with its reason recorded.
            (Prepared | Copying | CatchUp | Smoke | Handoff | Activated | Observing, Abandoned) => true,
            _ => false,
        }
    }
}

/// Where the current selector came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivationOrigin {
    /// The first selector of an installation, created with the genesis.
    Genesis { genesis_ref: GenesisRef },
    /// A later software generation, created by an activation.
    Activation { activation_id: EntityId },
}

/// The current control authority of one subject — SPEC §41.12 X05.
///
/// The successful conditional update of this record is the linearization
/// point of an activation. A symlink, a PID, or the version shown in a UI
/// never replaces it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationSelector {
    pub agent_id: EntityId,
    /// Monotonic CAS token. Every successful write adds exactly one.
    pub selector_revision: u64,
    /// Monotonic software/control-domain epoch. Never reused, never lowered.
    pub active_epoch: u64,
    /// The software generation that currently holds the control authority.
    pub generation: RuntimeGeneration,
    /// The durable state this generation reads and writes.
    pub state_ref: ObjectRef,
    pub origin: ActivationOrigin,
    /// The sealed route authority: which slot, artifact and launch command
    /// this generation owns.
    pub route_authority_ref: ObjectRef,
}

/// One software-generation hand-over — SPEC §41.12 X05.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationRecord {
    pub activation_id: EntityId,
    pub agent_id: EntityId,
    pub from_generation: RuntimeGeneration,
    pub to_generation: RuntimeGeneration,
    /// The retired generation's state copy.
    pub from_state_ref: ObjectRef,
    /// The candidate's state copy.
    pub to_state_ref: ObjectRef,
    /// The migrator that converts the stored schema, with its version.
    pub migrator_ref: VersionRef,
    /// A runnable closure that can still read or import the facts of both
    /// sides (old program and data, reader/importer for the new facts,
    /// object and secret handles, rollback plan, verification evidence).
    pub recovery_closure_ref: ObjectRef,
    pub contract_revision: String,
    pub phase: ActivationPhase,
    /// The self-consistent watermark the copy was taken at.
    pub w0: Option<CommitRef>,
    /// The final control-ledger watermark of the hand-over.
    pub w1: Option<CommitRef>,
    /// How far the candidate has actually imported.
    pub imported_through: Option<CommitRef>,
    pub ingress_cursors_ref: Option<ObjectRef>,
    pub evidence_cursors_ref: Option<ObjectRef>,
    pub pending_operations_ref: Option<ObjectRef>,
    pub inference_allocations_ref: Option<ObjectRef>,
    /// The selector revision this activation expects to swap.
    pub expected_selector_revision: u64,
    /// The epoch the selector will carry after the swap.
    pub target_epoch: u64,
    pub smoke_evidence_ref: Option<ObjectRef>,
    pub rollback_plan_ref: ObjectRef,
    pub last_error_ref: Option<ObjectRef>,
    /// Monotonic revision of this record; every accepted mutation adds one.
    pub record_revision: u64,
    /// The committed primary claim this hand-over was prepared under. It is
    /// re-read before the selector CAS; a changed claim refuses the swap.
    pub expected_primary_claim: Option<PrimaryClaim>,
}

/// The committed primary claim of the node domain — SPEC §5.4.
///
/// It says *who* is primary. The selector says *which software generation*
/// is current. The two epochs are separate domains and are never compared
/// with each other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrimaryClaim {
    pub term: u64,
    pub epoch: u64,
    pub leader_node_id: String,
    pub config_revision: u64,
    pub committed_seq: u64,
}

impl PrimaryClaim {
    /// The committed claim is the route authority only while it is unchanged.
    pub fn same_commitment(&self, other: &PrimaryClaim) -> bool {
        self.term == other.term
            && self.epoch == other.epoch
            && self.config_revision == other.config_revision
            && self.committed_seq == other.committed_seq
            && self.leader_node_id == other.leader_node_id
    }
}

/// What the product does with one stored schema during a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityAction {
    /// The candidate reads the stored format natively.
    ReadNative,
    /// The candidate migrates a copy of the stored format.
    MigrateCopy,
    /// A retained reader keeps the old format explainable.
    RetainReader,
    /// The affected unit is blocked and needs an autonomous repair path.
    BlockAffectedUnit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatibilityEntry {
    pub stored_schema: SchemaRef,
    pub action: CompatibilityAction,
    pub adapter: Option<VersionRef>,
    /// The units blocked by `BlockAffectedUnit`, with the repair path that is
    /// actually available.
    pub blocked_units: Vec<String>,
    pub repair_path: Option<String>,
}

/// A read-only expansion of one activation record and its recovery closure.
/// It is not a second activation state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationPlan {
    pub activation: ActivationRecord,
    pub compatibility: Vec<CompatibilityEntry>,
}

/// The sealed description of the route a generation owns.
///
/// It is written as a CAS object and referenced by
/// `ActivationSelector.route_authority_ref`, so "which slot and artifact are
/// live" is a durable fact with content identity, not a filename convention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteAuthority {
    pub schema_version: u32,
    pub agent_id: EntityId,
    pub generation: RuntimeGeneration,
    pub active_epoch: u64,
    pub selector_revision: u64,
    /// The live data directory of this generation.
    pub data_dir: String,
    /// The live slot directory holding this generation's state copy.
    pub slot_dir: String,
    /// The candidate artifact this generation runs, with its content digest.
    pub artifact_path: String,
    pub artifact_digest: Digest,
    /// The command that starts this generation.
    pub launch_command: Vec<String>,
    /// The committed primary claim this route was bound to, if any.
    pub primary_claim: Option<PrimaryClaim>,
    /// The activation that produced this route, absent for the genesis route.
    pub activation_id: Option<EntityId>,
}

impl RouteAuthority {
    pub const SCHEMA_VERSION: u32 = 1;
}
