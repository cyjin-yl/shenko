//! One parsed model result shared by every consumer. Raw bytes remain in CAS.
use crate::{identity::IdentityChange, ids::EntityId, object::ObjectRef};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputSpan {
    pub start: usize,
    pub end: usize,
    pub line: u64,
}

/// The sealed format version of a publication target.
pub const PUBLICATION_TARGET_SCHEMA_VERSION: u32 = 2;

/// Resolved reply destination: which registered channel, which of its
/// audience kinds and which account scope a reply control addresses.
///
/// `id` is an OPAQUE platform identity. A QQ number, an official OpenID, a
/// Feishu `chat_id` and an iLink context token are different namespaces; the
/// core stores them as strings and only the owning plugin interprets them
/// (SPEC §16, §35.6). `account` is the account/tenant namespace that owns
/// the conversation; `tenant` and `conversation` carry the platform's own
/// namespaces when it has them.
///
/// The record is explicitly versioned: `schema_version` is part of the sealed
/// bytes. Older sealed records (a numeric id, or the pre-plugin
/// `kind:"<channel>_<audience>"` form) are NOT accepted by this reader —
/// they are converted by the explicit Store migration, which writes the
/// versioned successor. There is no unversioned alias.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PublicationTarget {
    pub schema_version: u32,
    pub channel: String,
    pub kind: String,
    pub id: String,
    /// The account namespace that owns the conversation. An empty string marks
    /// an unscoped record (only produced by the migration of an old target);
    /// only a plugin bound to exactly one account may serve it, and it records
    /// the account it actually used.
    pub account: String,
    pub tenant: Option<String>,
    pub conversation: Option<String>,
}

impl PublicationTarget {
    /// A scoped target for one account.
    pub fn new(
        channel: impl Into<String>,
        kind: impl Into<String>,
        id: impl Into<String>,
        account: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: PUBLICATION_TARGET_SCHEMA_VERSION,
            channel: channel.into(),
            kind: kind.into(),
            id: id.into(),
            account: account.into(),
            tenant: None,
            conversation: None,
        }
    }

    /// Add the platform's own tenant/conversation namespaces.
    pub fn with_scope(
        mut self,
        tenant: Option<String>,
        conversation: Option<String>,
    ) -> Self {
        self.tenant = tenant;
        self.conversation = conversation;
        self
    }

    /// Convert one older sealed target into the versioned successor.
///
    /// This is the ONLY reader of the pre-v2 shapes and it exists for the
    /// explicit Store migration. It never guesses an account scope: an old
    /// record carried none, so the migrated target stays unscoped.
    pub fn read_legacy(value: &serde_json::Value) -> Option<Self> {
        let id = match value.get("id") {
            Some(serde_json::Value::Number(number)) => number.to_string(),
            Some(serde_json::Value::String(text)) if !text.is_empty() => text.clone(),
            _ => return None,
        };
        let (channel, kind) = match (
            value.get("channel").and_then(serde_json::Value::as_str),
            value.get("kind").and_then(serde_json::Value::as_str),
        ) {
            (Some(channel), Some(kind)) if !channel.is_empty() && !kind.is_empty() => {
                (channel.to_owned(), kind.to_owned())
            }
            // Pre-plugin form: the closed enum was stored as one tag.
            (None, Some(kind)) => {
                let (channel, kind) = kind.split_once('_')?;
                if channel.is_empty() || kind.is_empty() {
                    return None;
}
                (channel.to_owned(), kind.to_owned())
            }
            _ => return None,
        };
        Some(Self {
            schema_version: PUBLICATION_TARGET_SCHEMA_VERSION,
            channel,
            kind,
            id,
            account: String::new(),
            tenant: None,
            conversation: None,
                })
    }
}

impl<'de> Deserialize<'de> for PublicationTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Repr {
            schema_version: u32,
                channel: String,
                kind: String,
            id: String,
            account: String,
            #[serde(default)]
            tenant: Option<String>,
            #[serde(default)]
            conversation: Option<String>,
        }
        let repr = Repr::deserialize(deserializer)?;
        if repr.schema_version != PUBLICATION_TARGET_SCHEMA_VERSION {
            return Err(serde::de::Error::custom(format!(
                "unsupported publication target schema version {}",
                repr.schema_version
            )));
        }
        if repr.channel.is_empty() || repr.kind.is_empty() || repr.id.is_empty() {
            return Err(serde::de::Error::custom(
                "publication target needs channel, kind and id",
            ));
        }
        Ok(Self {
            schema_version: repr.schema_version,
            channel: repr.channel,
            kind: repr.kind,
            id: repr.id,
            account: repr.account,
            tenant: repr.tenant,
            conversation: repr.conversation,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ModelChildReplyGrant, PublicationTarget};

    /// The versioned target round-trips; older shapes are only readable
    /// through the explicit legacy reader used by the Store migration.
    #[test]
    fn only_the_versioned_target_deserializes() {
        let target = PublicationTarget::new("qq", "group", "12345", "qq:9");
        let bytes = serde_json::to_string(&target).unwrap();
        assert_eq!(
            serde_json::from_str::<PublicationTarget>(&bytes).unwrap(),
            target
        );
        assert!(serde_json::from_str::<PublicationTarget>(r#"{"channel":"qq","kind":"group","id":7}"#).is_err());
        assert!(serde_json::from_str::<PublicationTarget>(r#"{"kind":"relay_room","id":7}"#).is_err());
        let legacy = PublicationTarget::read_legacy(&serde_json::json!({"channel":"qq","kind":"group","id":7}))
            .unwrap();
        assert_eq!(legacy.id, "7");
        assert!(legacy.account.is_empty());
        let pre_plugin =
            PublicationTarget::read_legacy(&serde_json::json!({"kind":"relay_room","id":7})).unwrap();
        assert_eq!(pre_plugin.channel, "relay");
        assert_eq!(pre_plugin.kind, "room");
    }

    /// The audience kind is not an account. The grant binds the account
    /// namespace and refuses to contradict its own target.
    #[test]
    fn a_grant_binds_the_account_namespace() {
        let grant = |account: Option<&str>, target_account: &str| ModelChildReplyGrant {
            target: PublicationTarget::new("qq", "group", "12345", target_account),
            account: account.map(str::to_string),
            allowed_message_types: vec!["text".into()],
            reply_to_event_id: None,
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
        };
        assert_eq!(grant(None, "qq:9").account_namespace(), "qq:9");
        assert_eq!(grant(Some("qq:9"), "qq:9").account_namespace(), "qq:9");
        assert_eq!(grant(None, "").account_namespace(), "");
        assert_eq!(grant(Some("  "), "qq:9").account_namespace(), "qq:9");
        assert!(!grant(None, "qq:9").contradicts_target_account());
        assert!(!grant(Some("qq:9"), "qq:9").contradicts_target_account());
        assert!(!grant(Some("qq:9"), "").contradicts_target_account());
        assert!(grant(Some("qq:9"), "qq:10").contradicts_target_account());
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
    /// The account namespace this grant is bound to. Absent (or empty) means
    /// the target's own account is used.
    ///
    /// The audience kind (`group`, `private`, `feishu-chat`, ...) names WHICH
    /// kind of audience a platform id belongs to. It is never an account: the
    /// account names the connector instance that owns the conversation. A
    /// grant may name the account explicitly, but it may not contradict the
    /// account its own target carries.
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

impl ModelChildReplyGrant {
    /// The account namespace this grant is bound to.
    ///
    /// The publication target owns the account. An explicit grant account
    /// wins when it is present and non-empty; otherwise the target's own
    /// account is used, which may be empty for an unscoped record.
    pub fn account_namespace(&self) -> &str {
        match self.account.as_deref().map(str::trim) {
            Some(account) if !account.is_empty() => account,
            _ => self.target.account.as_str(),
        }
    }

    /// True when the grant names an account the target does not.
    ///
    /// Such a grant would send to one account while claiming another, so it
    /// is refused instead of resolved by guesswork.
    pub fn contradicts_target_account(&self) -> bool {
        let explicit = self.account.as_deref().map(str::trim).unwrap_or_default();
        let target = self.target.account.trim();
        !explicit.is_empty() && !target.is_empty() && explicit != target
    }
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
