//! One parsed model result shared by every consumer. Raw bytes remain in CAS.
use crate::{identity::IdentityChange, ids::EntityId, object::ObjectRef};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputSpan {
    pub start: usize,
    pub end: usize,
    pub line: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum PublicationTarget {
    QqGroup(u64),
    QqPrivate(u64),
}

/// Authored semantic task, not an endpoint, authority grant or tool schema.
/// The runtime binds the exact transport and enforces the sealed parent cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelChildSpec {
    pub goal: String,
    pub system_prompt: String,
    pub model: String,
    pub max_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutputAction {
    Reply {
        target: PublicationTarget,
        text: String,
    },
    Remember {
        text: String,
    },
    Delegate {
        text: String,
        /// Explicit child definition; no shell/SSH authority is exposed to the subject.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        definition: Option<crate::task::NativeTaskDefinition>,
    },
    DelegateModel {
        spec: ModelChildSpec,
    },
    Identity {
        change: IdentityChange,
    },
    /// The subject's own semantic wait decision. One wait control per result.
    SemanticWait {
        change: crate::subject::SemanticWaitChange,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputControl {
    pub action_id: EntityId,
    pub spans: Vec<OutputSpan>,
    pub action: OutputAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedOutput {
    pub schema_version: u32,
    pub step_id: EntityId,
    pub raw_ref: ObjectRef,
    /// Natural-language text, with active control syntax removed. No rewriting.
    pub text: String,
    pub controls: Vec<OutputControl>,
}
