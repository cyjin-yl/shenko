//! Awareness framework — docs/specs/cognition-units.md#spec-45.
//! Installable cognition units (perception, awareness, Skill, code) with a
//! shared installation authority, and SelfKnowledgeProvider — docs/specs/cognition-units.md#spec-38
//! built-in self-explanation.
pub mod awareness_host;
pub mod cognition;
pub mod perception;
pub mod self_knowledge;

pub use awareness_host::*;
pub use cognition::*;
pub use perception::*;
pub use self_knowledge::*;
