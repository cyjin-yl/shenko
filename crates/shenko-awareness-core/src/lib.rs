//! Awareness framework — SPEC §45.
//! Independently installable awareness units with source + applicability.
//! SelfKnowledgeProvider — SPEC §38 built-in self-explanation.
pub mod unit;
pub mod registry;
pub mod self_knowledge;
pub use unit::*;
pub use registry::*;
pub use self_knowledge::*;
