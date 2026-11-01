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
    use super::PublicationTarget;

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
