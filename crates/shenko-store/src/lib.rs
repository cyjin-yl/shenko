//! Shenko persistent store — SQLite WAL, docs/specs/durable-state.md#spec-41-2
//! One mutation line, one current writer incarnation per local ledger.
//! Configures: journal_mode=WAL, synchronous=FULL, foreign_keys=ON
pub mod migrations;
pub mod store;
pub mod activation;
pub mod cas;
pub mod ledger;
pub mod memory;
pub mod research;
pub mod identity;
pub mod session_events;
pub mod context_windows;
pub mod context_notes;
pub mod raw_session;
pub mod cognition;
pub mod atoms;
pub mod cognition_archive;
pub mod contacts;
pub mod recovery_sources;
pub mod semantic_wait;
pub mod seat;
pub mod runtime_boundary;
pub mod web_input;
pub mod subject_request;
pub mod subject_state;
pub mod cognitive_root;
/// Durable reconciliation cases for unknown external effects — docs/specs/durable-state.md#spec-41-4 X11.
pub mod reconcile;
/// Cognitive atoms: content registration, attitudes, reason pointers — docs/specs/cognition-units.md#spec-51-1, docs/specs/cognition-units.md#spec-51-3.
pub use reconcile::{ReconciliationDocketEntry, REASON_EFFECT_UNKNOWN};pub use subject_request::{
    SubjectRequestAttempt, SubjectRequestState, SubjectRequestTerminal,
};
pub mod output_parts;
pub mod config;
pub mod artifacts;

pub use store::*;
pub use activation::*;
pub use cas::*;
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

/// Receipt admission identity and source-event dedup — SPEC X04, docs/specs/plugins.md#spec-41-7-3.
pub mod receipt_admission;
pub use receipt_admission::{ConflictIdentity, EvidenceReceiptConflict};

pub mod child;
pub use child::{NativeTaskClaim, TaskCancellation};

pub mod intents;

/// Channel publication facts, the shared channel operation ledger and the
/// explicit publication-target migration. docs/specs/channel-adapters.md#spec-15-3, docs/specs/channels.md#spec-35-6, docs/specs/channels.md#spec-41-6.
pub mod channel;
pub use channel::{ArmedPublication, ChannelFact, NewChannelFact};

pub mod media;
pub mod quota;

/// Durable source-bound content stream journal — docs/specs/durable-state.md#content-stream-journal.
pub mod content_stream;
pub use content_stream::{
    CONTENT_CHUNK_MAX_BYTES, CONTENT_FLUSH_MAX_MILLIS, CONTENT_WHOLE_MAX_BYTES,
    ContentStreamBatcher, ContentStreamRecord, ContentStreamUnit, provider_stream_id,
};
