//! Factual first-native-observation delivery. These are not control grants.
use serde::{Deserialize, Serialize};

use crate::{
    commit::CommitRef,
    ids::EntityId,
    object::{Digest, ObjectRef},
};

pub const NATIVE_EVIDENCE_SCOPE: &str = "canonical_native_first_observation.v1";
pub const NATIVE_EVIDENCE_PAGE_ITEMS: usize = 16;
pub const NATIVE_EVIDENCE_CLOSURE_OBJECTS: usize = 4096;
pub const NATIVE_EVIDENCE_CLOSURE_BYTES: u64 = 64 * 1024 * 1024;

/// A physical enumeration position belongs to the serving installation. It
/// must never replace an observation's original received_commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEvidencePosition {
    pub sequence: u64,
    pub receipt_id: EntityId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEvidenceCursor {
    pub ledger_id: EntityId,
    pub after: Option<NativeEvidencePosition>,
    /// Fixed inclusive physical upper bound, chosen on the first page.
    pub through: u64,
}

/// Supplied by the authenticated Node adapter, never by the E1 producer's
/// unverified source_id string. A later branch may still deliver old facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEvidenceSource {
    pub node_id: EntityId,
    pub ledger_id: EntityId,
    pub agent_id: EntityId,
    pub branch_id: EntityId,
    pub origin_proof: Digest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeEvidenceAvailability {
    Sealed { evidence_ref: ObjectRef },
    Gap { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEvidenceItem {
    pub position: NativeEvidencePosition,
    pub operation_id: EntityId,
    pub attempt_id: EntityId,
    pub source_commit: CommitRef,
    pub availability: NativeEvidenceAvailability,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEvidencePage {
    pub schema_version: u32,
    pub scope: String,
    pub source: NativeEvidenceSource,
    pub cursor: NativeEvidenceCursor,
    pub items: Vec<NativeEvidenceItem>,
    /// None means this bounded snapshot was scanned, not that every fact was
    /// applied or that this peer contains all external observations.
    pub next: Option<NativeEvidenceCursor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEvidenceDelivery {
    pub source: NativeEvidenceSource,
    pub item: NativeEvidenceItem,
}
