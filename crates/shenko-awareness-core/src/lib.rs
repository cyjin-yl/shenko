//! Awareness framework — SPEC §45.
//! Independently installable awareness units with source + applicability.
//! SelfKnowledgeProvider — SPEC §38 built-in self-explanation.
pub mod awareness_unit;
pub mod self_knowledge;

pub use awareness_unit::*;
pub use self_knowledge::*;
