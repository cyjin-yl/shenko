//! Bounded original-native-observation delivery. These are not control grants.
use serde::{Deserialize, Serialize};

use crate::{
    commit::CommitRef,
    ids::EntityId,
    object::{Digest, ObjectRef},
};

pub const NATIVE_EVIDENCE_SCOPE: &str = "canonical_native_first_and_unknown_resolution.v2";
pub const NATIVE_EVIDENCE_SCHEMA_VERSION: u32 = 2;
pub const NATIVE_EVIDENCE_PAGE_ITEMS: usize = 16;
pub const NATIVE_EVIDENCE_CLOSURE_OBJECTS: usize = 4096;
pub const NATIVE_EVIDENCE_CLOSURE_BYTES: u64 = 64 * 1024 * 1024;
pub const NATIVE_EVIDENCE_CODEC: &str = "store-native-evidence-fact";
pub const NATIVE_EVIDENCE_ANCHORED_CODEC_MAJOR: u32 = 6;

/// Exact graph authority of an object reference, independent of its mutable
/// transport locator. A digest seen as opaque bytes does not satisfy a later
/// JSON or stronger-schema reference to those same bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativeEvidenceObjectIdentity {
    pub digest: Digest,
    pub bytes: u64,
    pub media_type: String,
    pub store_namespace: String,
    pub codec_name: String,
    pub codec_major: u32,
    pub codec_digest: Digest,
}

impl From<&ObjectRef> for NativeEvidenceObjectIdentity {
    fn from(reference: &ObjectRef) -> Self {
        Self {
            digest: reference.digest.clone(),
            bytes: reference.bytes,
            media_type: reference.media_type.clone(),
            store_namespace: reference.store_namespace.clone(),
            codec_name: reference.codec.name.clone(),
            codec_major: reference.codec.major,
            codec_digest: reference.codec.digest.clone(),
        }
    }
}

/// Version 6 measures only the new observation graph against E1 transport
/// capacity. Its predecessors must already be closed and verified locally;
/// they remain full graph edges for archives and GC. This envelope is inert:
/// only Store can validate the inner fact and its actual local T1/source/E1
/// applications. A sender-supplied predecessor list is never such authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnchoredNativeEvidence {
    pub schema_version: u32,
    /// Immutable, complete original-format fact (codec 2, 3, 4 or 5).
    pub fact_ref: ObjectRef,
    /// Canonically ordered by NativeEvidenceObjectIdentity, without aliases.
    /// Store independently derives exactly T1 payload, optional source E1,
    /// and optional original Unknown E1 from fact_ref.
    pub predecessor_roots: Vec<ObjectRef>,
}

impl AnchoredNativeEvidence {
    /// Decode the transport envelope without granting factual authority.
    pub fn decode(reference: &ObjectRef, bytes: &[u8]) -> Result<Self, String> {
        let typed = |reference: &ObjectRef, name: &str, major: u32| {
            reference.digest.canonical_blake3_hex().is_some()
                && reference.media_type == "application/json"
                && reference.store_namespace == "commit-authority"
                && reference.codec.name == name
                && reference.codec.major == major
                && reference.codec.digest == Digest::blake3(name.as_bytes())
        };
        if !typed(reference, NATIVE_EVIDENCE_CODEC, NATIVE_EVIDENCE_ANCHORED_CODEC_MAJOR)
            || reference.bytes > NATIVE_EVIDENCE_CLOSURE_BYTES
            || reference.bytes != bytes.len() as u64
            || reference.digest != Digest::blake3(bytes)
        {
            return Err("anchored E1 reference or bytes differ".into());
        }
        let envelope: Self = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        if envelope.schema_version != NATIVE_EVIDENCE_ANCHORED_CODEC_MAJOR
            || !matches!(envelope.fact_ref.codec.major, 2..=5)
            || !typed(&envelope.fact_ref, NATIVE_EVIDENCE_CODEC, envelope.fact_ref.codec.major)
            || envelope.fact_ref.digest == reference.digest
            || !(1..=3).contains(&envelope.predecessor_roots.len())
        {
            return Err("anchored E1 fact or predecessor shape differs".into());
        }
        let mut previous = None;
        for root in &envelope.predecessor_roots {
            let identity = NativeEvidenceObjectIdentity::from(root);
            if !(typed(root, "commit-admission", 1)
                || (matches!(root.codec.major, 1..=6)
                    && typed(root, NATIVE_EVIDENCE_CODEC, root.codec.major)))
                || root.digest == reference.digest
                || root.digest == envelope.fact_ref.digest
                || previous.as_ref().is_some_and(|previous| previous >= &identity)
            {
                return Err("anchored E1 predecessor identity or canonical order differs".into());
            }
            previous = Some(identity);
        }
        Ok(envelope)
    }
}

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
    /// A cursor from the old first-only scope cannot skip a successor prefix.
    pub scope: String,
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
