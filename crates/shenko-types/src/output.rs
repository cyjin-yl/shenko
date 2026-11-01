//! One parsed model result shared by every consumer. Raw bytes remain in CAS.
use crate::{identity::IdentityChange, ids::EntityId, object::ObjectRef};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputSpan {
    pub start: usize,
    pub end: usize,
    pub line: u64,
}

/// Resolved reply destination: which registered channel and which of its
/// audience kinds a reply control addresses. `channel` and `kind` name the
/// channel plugin's own id and reply tag (see `crate::channel`); `id` is the
/// platform's positive integer account for that audience.
///
/// Wire compatibility: new records serialize as
/// `{"channel":"…","kind":"…","id":N}`. Records sealed before this shape
/// stored the closed enum as `{"kind":"<channel>_<audience>","id":N}`; this
/// reader keeps accepting that legacy form by splitting the tag on the first
/// `_`. New-target admission is enforced by the channel registry at the
/// parser/outbound edge, never here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PublicationTarget {
    pub channel: String,
    pub kind: String,
    pub id: u64,
}

impl PublicationTarget {
    pub fn new(channel: impl Into<String>, kind: impl Into<String>, id: u64) -> Self {
        Self {
            channel: channel.into(),
            kind: kind.into(),
            id,
        }
    }
}

impl<'de> Deserialize<'de> for PublicationTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Current {
                channel: String,
                kind: String,
                id: u64,
            },
            /// Pre-plugin sealed form: `{"kind":"<channel>_<audience>","id":N}`.
            Legacy { kind: String, id: u64 },
        }
        match Repr::deserialize(deserializer)? {
            Repr::Current { channel, kind, id }
                if !channel.is_empty() && !kind.is_empty() =>
            {
                Ok(PublicationTarget { channel, kind, id })
            }
            Repr::Legacy { kind, id } => {
                let (channel, kind) = kind
                    .split_once('_')
                    .filter(|(channel, kind)| !channel.is_empty() && !kind.is_empty())
                    .ok_or_else(|| serde::de::Error::custom("unknown publication target"))?;
                Ok(PublicationTarget {
                    channel: channel.to_owned(),
                    kind: kind.to_owned(),
                    id,
                })
            }
            _ => Err(serde::de::Error::custom(
                "publication target needs channel and kind",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PublicationTarget;

    /// Sealed pre-plugin records `{kind:"<channel>_<audience>",id}` still
    /// deserialize; the channel id is recovered from the legacy tag.
    #[test]
    fn legacy_and_current_publication_targets_round_trip() {
        let legacy_room: PublicationTarget =
            serde_json::from_str(r#"{"kind":"relay_room","id":7}"#).unwrap();
        assert_eq!(legacy_room, PublicationTarget::new("relay", "room", 7));
        let target = PublicationTarget::new("relay", "room", 9);
        let bytes = serde_json::to_string(&target).unwrap();
        assert_eq!(
            serde_json::from_str::<PublicationTarget>(&bytes).unwrap(),
            target
        );
        // Records that never matched either sealed shape still fail.
        assert!(
            serde_json::from_str::<PublicationTarget>(r#"{"kind":"tagged","id":1}"#).is_err()
        );
        assert!(serde_json::from_str::<PublicationTarget>(r#"{"id":1}"#).is_err());
    }
}

/// One tool the child may call. It names a registered native definition; the
/// runtime binds the real handler, its authority and its own execution
/// contract. An authored kind is not a capability until the registry has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelChildTool {
    pub kind: String,
    pub definition_version: u32,
}

/// One authored context block. The child receives it with its own label, so
/// the delegator can hand over material without editing the child's prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelChildContext {
    pub label: String,
    pub text: String,
}

/// The child's own finite budget. It is allocated by the delegator; a nested
/// child may only receive what its parent has left (SPEC §14.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelChildBudget {
    /// Output tokens the whole run may reserve across all of its steps.
    pub max_total_tokens: u64,
    /// Model steps the run may take.
    pub max_steps: u32,
    /// Tool calls the run may make.
    pub max_tool_calls: u32,
}

/// The reply authority a delegator may hand to a model child. The runtime
/// binds it to the child's own task identity at freeze time; the channel
/// plugin interprets the target. Without such a grant the child cannot
/// publish: an ordinary child has no outbound authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelChildReplyGrant {
    /// The resolved destination. The channel plugin owns this namespace.
    pub target: PublicationTarget,
    /// The account this grant is bound to. Absent means the target's own
    /// audience kind is the account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Message types the child may send on this grant.
    pub allowed_message_types: Vec<String>,
    /// The event this grant answers, when the delegator names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_event_id: Option<crate::ids::EntityId>,
    /// The grant stops working at this instant.
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

impl ModelChildBudget {
    /// Default step count of a child that did not author one.
    pub const DEFAULT_MAX_STEPS: u32 = 8;

    /// The effective budget of a spec. An authored budget is used as written;
    /// otherwise the run gets the finite default derived from the child's own
    /// per-step output budget. The resolution lives here so the subject that
    /// freezes the contract and the store that admits it agree on one number.
    pub fn resolve(spec: &ModelChildSpec) -> ModelChildBudget {
        match &spec.budget {
            Some(budget) => budget.clone(),
            None => ModelChildBudget {
                max_total_tokens: spec.max_tokens.saturating_mul(u64::from(Self::DEFAULT_MAX_STEPS)),
                max_steps: Self::DEFAULT_MAX_STEPS,
                max_tool_calls: Self::DEFAULT_MAX_STEPS,
            },
        }
    }

    /// Integer limits are finite; the author chooses their magnitude. Parent
    /// allowance and actual resource admission remain separate constraints.
    pub fn is_finite(&self) -> bool {
        self.max_steps > 0 && self.max_total_tokens > 0
    }
}

/// Authored semantic task, not an endpoint, authority grant or tool schema.
/// The runtime binds the exact transport and enforces the child's own budget.
///
/// `max_tokens` is the child's per-step output budget. It is the child's own
/// number: it is not mechanically capped by the delegating step's single-turn
/// output cap (SPEC §6). Only real transport facts and the child's own
/// budget bound it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelChildSpec {
    pub goal: String,
    pub system_prompt: String,
    pub model: String,
    pub max_tokens: u64,
    /// The tools this child may call. Empty means a text-only child.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ModelChildTool>,
    /// Authored context handed to the child next to its goal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<ModelChildContext>,
    /// The run's finite budget. Absent means the finite default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<ModelChildBudget>,
    /// The outbound authority this child may use. Absent means the child
    /// cannot publish at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_grant: Option<ModelChildReplyGrant>,
    /// Continue an earlier finite child run instead of starting a new one.
    ///
    /// The named task must be a finished model child of the same subject.
    /// Its recorded transcript is reused and its already executed tool calls
    /// are read, never run again: a continuation is the same discussion, not
    /// a second child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_task_id: Option<crate::ids::EntityId>,
    /// The owner's answer to the question the continued child asked. It is
    /// the owner's own words, delivered as the next user turn of the same
    /// discussion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_answer: Option<String>,
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
