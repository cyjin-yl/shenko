//! Software-generation activation: built candidates, candidate slots, the
//! durable selector, the subject-quiesce hand-over and rollback.
//!
//! SPEC §9/§10/§44 and the self-update contract (docs/specs/self-update.md)
//! require the update to be persisted on separate copies, smoke-launched,
//! swapped through a compare-and-swap selector with a strictly increasing
//! epoch, and recoverable without a model.
//!
//! The module owns: the build provider (`build`), candidate slot layout
//! (`slots`), diff-based migration (`migration`), the smoke launch
//! (`smoke`), the orchestration of quiesce → catch-up → CAS → fence →
//! route → resume (`handoff`), the cold-start reconciliation of a hand-over
//! that died before its selector CAS (`handoff`), the `update.control` native
//! handler (`native`), the §51.4 append-node cognitive-root rollback
//! (`rollback`) and the deterministic CLI (`cli`). The concrete participants
//! (playground, wasm home, node) register through the
//! [`handoff::HandoffParticipantRegistry`] seam.
//!
//! Two different rollbacks live here and must not be confused: `handoff`
//! rolls the selected *software generation* back by creating a new activation
//! record and importing the newer generation's facts into the retained copy
//! (§9/§44), while `rollback` restores the subject's *state root* by appending
//! a new root node whose content equals a selected historical node (§51.4).

pub mod build;
pub mod cli;
pub mod handoff;
pub mod migration;
pub mod native;
pub mod rollback;
pub mod slots;
pub mod smoke;

pub use slots::SLOT_DB_FILENAME;
