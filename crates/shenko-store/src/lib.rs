//! Shenko persistent store — SQLite WAL
//! One mutation line, one current writer incarnation per local ledger.
//! Configures: journal_mode=WAL, synchronous=FULL, foreign_keys=ON
pub mod migrations;
pub mod store;
#[cfg(feature = "test-support")]
pub mod test_support;
pub mod authority;
mod control_recovery;
mod control_transition;
mod dispatch_transition;
mod arming_transition;
mod claim_transition;
mod native_transition;
mod native_facts;
mod native_evidence;
mod native_evidence_successor;
pub use native_evidence::NativeEvidenceSealDebt;
mod native_evidence_delivery;
pub use native_evidence_delivery::{NativeEvidenceDeliveryOutcome, NativeEvidenceDeliveryRecord, NativeEvidenceDeliveryState};
pub use control_recovery::{ControlRecoveryOutcome, PreparedStepDebt, PreparedStepRecovery, CONTROL_RECONCILIATION_SOURCE};
mod control_predecessor;
pub use control_predecessor::ControlPredecessorEvidence;
pub mod activation;
pub mod local_model_health;
mod local_installation;
pub use local_installation::{
    HomeInstallationBinding, LocalInstallationProvisioning, LOCAL_HOME_CODEC, LOCAL_HOME_NAMESPACE,
};
pub mod cas;
/// Shared-CAS roots, publication leases and retained owner exclusions.
pub mod roots;
pub mod ledger;
pub mod memory;
pub mod memory_git_ack;
pub mod research;
pub mod identity;
pub mod session_events;
pub mod context_windows;
mod context_correction;
pub mod context_notes;
pub mod raw_session;
pub mod cognition;
pub mod atoms;
pub mod cognition_archive;
pub mod contacts;
pub mod recovery_sources;
pub mod retained_objects;
pub use retained_objects::{RetainedObjectSnapshot, RetentionLimits};
pub mod semantic_wait;
pub mod seat;
pub mod runtime_boundary;
pub mod web_attachment;
pub mod web_input;
pub mod subject_request;
pub mod subject_state;
pub mod subject_branches;
pub use subject_branches::SubjectBranchFork;
pub mod cognitive_root;
/// Durable reconciliation cases for unknown external effects — X11.
pub mod reconcile;
/// Cognitive atoms: content registration, attitudes, reason pointers.
pub use reconcile::{ReconciliationDocketEntry, REASON_EFFECT_UNKNOWN};pub use subject_request::{
    SubjectRequestAttempt, SubjectRequestState, SubjectRequestTerminal,
};
pub mod output_parts;
pub mod config;
pub mod artifacts;

pub use store::*;
pub use activation::*;
pub use cas::*;
pub use roots::*;
pub use ledger::*;
pub use memory::*;
pub use research::*;
pub use session_events::*;
pub use context_windows::*;
pub use context_notes::*;
pub use raw_session::*;
pub use cognition::*;
pub use atoms::*;
pub use cognition_archive::*;
pub use contacts::*;
pub use semantic_wait::*;
pub use seat::*;
pub use config::*;
pub use artifacts::*;

pub mod external;
pub use external::InboxAdmission;

/// Receipt admission identity and source-event dedup — SPEC X04.
pub mod receipt_admission;
pub use receipt_admission::{ConflictIdentity, EvidenceReceiptConflict};

pub mod child;
pub use child::{NativeTaskClaim, TaskCancellation};
mod model_child_budget;
pub use model_child_budget::{ModelChildBudgetExceeded, ModelChildBudgetUsage};

pub mod intents;

/// Channel publication facts, the shared channel operation ledger and the
/// explicit publication-target migration.
pub mod channel;
pub use channel::{ArmedPublication, ChannelFact, NewChannelFact};

pub mod media;
pub mod quota;

/// Durable source-bound content stream journal.
pub mod content_stream;
pub mod trajectory;
pub use content_stream::{
    CONTENT_CHUNK_MAX_BYTES, CONTENT_FLUSH_MAX_MILLIS, CONTENT_WHOLE_MAX_BYTES,
    ContentStreamBatcher, ContentStreamRecord, ContentStreamUnit, provider_stream_id,
};
