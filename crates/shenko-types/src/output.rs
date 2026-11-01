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

/// Authored disposition of one durable input event. The sealed decision binds
/// it to the exact event selected into this step's S0 (SPEC §13.8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispositionSpec {
    pub event_id: EntityId,
    pub disposition: crate::subject::Disposition,
    /// Why the model disposed it this way. Required unless acknowledged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<crate::subject::EventPriority>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_after: Option<chrono::DateTime<chrono::Utc>>,
}

/// A durable obligation the subject takes on: it stays ready and is read
/// back into a later S0 until completed or explicitly cancelled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObligationSpec {
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on_task_id: Option<EntityId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_condition: Option<String>,
    /// The durable event this obligation answers; required when the
    /// obligation answers a specific input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_event_id: Option<EntityId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Authored dialogue turn to a durable task (usually a model child).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogueSpec {
    pub target_task_id: EntityId,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<EntityId>,
    /// Defaults to the child direction; `to_subject` is how a child answers
    /// its parent and is not authored by the subject loop.
    #[serde(default)]
    pub direction: crate::subject::DialogueDirection,
}

/// The subject's own attention decision (do-not-disturb and back). A switch
/// off requires the sources it silences and a review time; it is never an
/// unbounded silent switch (SPEC §13.8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttentionSpec {
    pub state: crate::subject::AttentionState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_after: Option<chrono::DateTime<chrono::Utc>>,
}

/// The subject's adoption of one live steer that was carried into this
/// step's sealed request. The submission key must name a steer that was
/// actually selected into this S0; the store re-checks that binding at S1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SteerSpec {
    pub submission_id: String,
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
    /// Authored disposition of one selected input event.
    Dispose {
        spec: DispositionSpec,
    },
    /// A durable obligation taken on by this step.
    Obligation {
        spec: ObligationSpec,
    },
    /// An authored dialogue turn to a durable task.
    Dialogue {
        spec: DialogueSpec,
    },
    /// Attention switch (do-not-disturb / resume).
    Dnd {
        spec: AttentionSpec,
    },
    /// The subject adopted one live steer carried into this step.
    Steer {
        spec: SteerSpec,
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
