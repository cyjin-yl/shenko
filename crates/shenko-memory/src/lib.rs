//! Shenko memory — durable intent→committed chain, replaceable backend,
//! resident worker, Git text knowledge and match-centred recall.
//!
//! SPEC §12.2/§12.3/§12.4: memory requests are admitted durably before any
//! entry exists, processed by a real worker, searched with Chinese-aware
//! retrieval and match-centred excerpts, semantically forgotten with
//! monotonic tombstones, corrected with real supersession, and maintained as
//! explainable text commits in the managed Git knowledge repository
//! (SPEC §10.3/§10.5). Operation kinds and open knowledge-kind strings are
//! separate at every layer.
pub mod backend;
pub mod excerpt;
pub mod knowledge;
pub mod memory_store;
pub mod worker;

pub use backend::{
    FlushOutcome, ForgetOutcome, GitHealth, MemoryBackend, MemoryHealth, MemoryHit,
    MemoryRequest, MemorySupersessionStatus, RecallQuery, RetainOutcome, StoreMemoryBackend,
};
pub use excerpt::{excerpt_for, MatchExcerpt};
pub use knowledge::GitKnowledgeStore;
pub use memory_store::{AcceptedRef, MemoryStore, ProcessOutcome, QueuedIntent};
pub use worker::{MemoryWorker, MemoryWorkerPlugin};
