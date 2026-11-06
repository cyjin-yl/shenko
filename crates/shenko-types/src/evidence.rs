//! 证据收据与对账案例 — SPEC §41.4 X04

use serde::{Deserialize, Serialize};

use crate::commit::CommitRef;
use crate::ids::EntityId;
use crate::object::{ObjectRef, VersionRef};

/// 观察级别
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationLevel {
    TransportWritten,
    ProviderAccepted,
    RemoteCompleted,
    PostconditionVerified,
}

/// EvidenceReceipt — SPEC §41.4 X04
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceReceipt {
    pub record_schema_version: u32,
    pub receipt_id: EntityId,
    pub operation_id: Option<EntityId>,
    pub attempt_id: Option<EntityId>,
    pub source_id: String,
    pub source_instance_id: String,
    pub source_event_key: Option<String>,
    pub producer_epoch: Option<u64>,
    pub producer_schema_version: Option<String>,
    pub raw_ref: ObjectRef,
    pub decoder_ref: Option<VersionRef>,
    pub decode_status: DecodeStatus,
    pub observation_level: Option<ObservationLevel>,
    pub normalized_ref: Option<ObjectRef>,
    pub observed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub received_commit: CommitRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecodeStatus {
    Decoded,
    Uninterpreted,
    Invalid,
}

impl EvidenceReceipt {
    /// The source scope a stable event key deduplicates inside: source,
    /// source instance and the producer's own event key (SPEC X04,
    /// §41.7.3). Without a stable key there is nothing to deduplicate
    /// against, and equal content alone is not proof that two external
    /// events are the same event.
    pub fn source_scope(&self) -> Option<(&str, &str, &str)> {
        self.source_event_key.as_deref().map(|key| {
            (
                self.source_id.as_str(),
                self.source_instance_id.as_str(),
                key,
            )
        })
    }

    /// Whether two receipts describe the SAME original observation.
    ///
    /// The original is the source-side fact: what the producer observed and
    /// how it was decoded. The local `receipt_id` and the `received_commit`
    /// this store assigned when it admitted the first copy of that original
    /// are the store's own bookkeeping, so a producer that resends the same
    /// original under a new local id is still the same event; a resend that
    /// changes any source-side fact is a different original.
    pub fn same_source_original(&self, other: &Self) -> bool {
        self.record_schema_version == other.record_schema_version
            && self.source_id == other.source_id
            && self.source_instance_id == other.source_instance_id
            && self.source_event_key == other.source_event_key
            && self.producer_epoch == other.producer_epoch
            && self.producer_schema_version == other.producer_schema_version
            && self.raw_ref == other.raw_ref
            && self.decoder_ref == other.decoder_ref
            && self.decode_status == other.decode_status
            && self.observation_level == other.observation_level
            && self.normalized_ref == other.normalized_ref
            && self.observed_at == other.observed_at
    }

    /// The receipt facts that differ between two copies of one identity.
    /// A conflict returns this summary instead of overwriting either side.
    /// `received_commit` is excluded: it is assigned by the store that
    /// admitted the copy, not observed at the source.
    pub fn divergent_fields(&self, other: &Self) -> Vec<&'static str> {
        let mut fields = Vec::new();
        if self.receipt_id != other.receipt_id {
            fields.push("receipt_id");
        }
        if self.operation_id != other.operation_id {
            fields.push("operation_id");
        }
        if self.attempt_id != other.attempt_id {
            fields.push("attempt_id");
        }
        if self.record_schema_version != other.record_schema_version {
            fields.push("record_schema_version");
        }
        if self.source_id != other.source_id {
            fields.push("source_id");
        }
        if self.source_instance_id != other.source_instance_id {
            fields.push("source_instance_id");
        }
        if self.source_event_key != other.source_event_key {
            fields.push("source_event_key");
        }
        if self.producer_epoch != other.producer_epoch {
            fields.push("producer_epoch");
        }
        if self.producer_schema_version != other.producer_schema_version {
            fields.push("producer_schema_version");
        }
        if self.raw_ref != other.raw_ref {
            fields.push("raw_ref");
        }
        if self.decoder_ref != other.decoder_ref {
            fields.push("decoder_ref");
        }
        if self.decode_status != other.decode_status {
            fields.push("decode_status");
        }
        if self.observation_level != other.observation_level {
            fields.push("observation_level");
        }
        if self.normalized_ref != other.normalized_ref {
            fields.push("normalized_ref");
        }
        if self.observed_at != other.observed_at {
            fields.push("observed_at");
        }
        fields
    }
}

/// One preserved `RECEIPT_KEY_CONFLICT` (SPEC X04, §41.7.3).
///
/// The first receipt of a source scope keeps its identity unchanged; a
/// divergent original that arrives under the same scope (or under an
/// existing receipt id) is kept here, with its own facts, instead of being
/// dropped or written over the original. This is local admission
/// bookkeeping: it is not a new replicated/exported fact class.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceKeyConflict {
    pub schema_version: u32,
    pub conflict_id: EntityId,
    pub kind: EvidenceConflictKind,
    /// `source\0instance\0key` for a scoped key conflict; the receipt id for
    /// an id conflict.
    pub scope_key: String,
    pub first_receipt_id: EntityId,
    pub first_operation_id: Option<EntityId>,
    pub conflicting_receipt_id: EntityId,
    pub conflicting_operation_id: Option<EntityId>,
    /// Fields whose stored and incoming values disagree.
    pub divergent_fields: Vec<String>,
    /// The divergent original exactly as it was delivered.
    pub original: EvidenceReceipt,
    pub detected_commit: CommitRef,
    pub detected_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceConflictKind {
    /// Two originals share one receipt identity.
    ReceiptId,
    /// Two originals share one source scope and stable event key.
    SourceEventKey,
}

/// 对账案例 — SPEC §41.4 X04
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationCase {
    pub case_id: EntityId,
    pub operation_id: EntityId,
    pub supervisor_task_id: EntityId,
    pub reason: String,
    pub state: ReconciliationState,
    pub evidence_refs: Vec<EntityId>,
    pub next_action_ref: Option<ObjectRef>,
    pub decision_ref: Option<ObjectRef>,
    pub created_commit: CommitRef,
    pub updated_commit: CommitRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationState {
    Open,
    Working,
    Waiting,
    Resolved,
    AcknowledgedUnknown,
}
