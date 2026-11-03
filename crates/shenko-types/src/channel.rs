//! IM-channel plugin boundary — SPEC §15–16, §35.6, §41.6.
//!
//! One channel plugin owns everything that is platform-specific about a
//! messaging channel:
//!
//! * **inbound classification** — the raw provider payload becomes one
//!   normalized [`ChannelEnvelope`] with explicit account/tenant/conversation
//!   scope, a normalized actor and parsed attachments;
//! * **envelope projection** — the plugin names the replyable audience and its
//!   human label, so the subject core never parses a platform id;
//! * **outbound description** — a sealed [`PublicationTarget`] becomes one
//!   [`ChannelDispatch`] with the platform action, parameters and acceptance
//!   predicate;
//! * **outbound execution** — the plugin performs the wire call and returns a
//!   [`ChannelReceipt`].
//!
//! The durable outbox, the T1 arm and the E1 receipt are NOT plugin business:
//! they live in the Store so every channel shares one operation ledger.
//!
//! Identities are opaque strings. A QQ number, an official OpenID, a Feishu
//! `chat_id` and an iLink context token are different namespaces; the core
//! stores them exactly as the platform reported them and lets the owning
//! plugin interpret them. The subject core names no platform at all.
//!
//! Ordinary reachability is not authority: a group owner, a transport token
//! or a familiar face never makes an actor an administrator. Administrative
//! role is an owner-set fact carried separately ([`ChannelActor::role_grant_ref`]).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::object::Digest;
use crate::output::PublicationTarget;

/// A boxed future returned by plugin-owned asynchronous work.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One audience kind a channel can deliver a reply to.
///
/// `tag` is the middle segment of `[reply:<tag>:<id>]`; `id_hint` is the
/// localized placeholder printed in the reply grammar (e.g. a group number or
/// account name); `audience` is the human noun phrase used in the
/// sender-label grammar line. The id itself is an opaque string owned by the
/// plugin — the core never parses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplyKind {
    pub tag: &'static str,
    pub id_hint: &'static str,
    pub audience: &'static str,
}

/// The normalized inbound envelope every channel plugin produces.
///
/// The raw provider payload is kept verbatim next to it (`raw_ref` in the
/// ingress ledger); this struct is the classified, namespace-explicit view the
/// subject and the durable facts are built from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelEnvelope {
    /// Envelope format version. Bumped when the meaning of a field changes.
    pub schema_version: u32,
    /// The channel plugin that classified this event (`channel_id`).
    pub provider: String,
    /// Account namespace that owns the conversation, e.g. `qq:9`.
    pub account: String,
    /// Platform tenant namespace when the platform has one (app tenant).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// The identity domain of the transport that delivered the event. It is
    /// part of the strong dedup key together with provider and account.
    pub transport_identity_domain: String,
    /// Conversation identity (group, chat, thread) when the platform names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    /// Thread identity inside a conversation, when the platform has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// The platform's own event/message identity, opaque to the core.
    pub message_id: String,
    /// The platform's revision marker for an edited message, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_event_revision: Option<String>,
    /// When the platform says the event happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When this runtime received the event.
    pub received_at: chrono::DateTime<chrono::Utc>,
    /// Who sent it, as far as the platform states it.
    pub actor: ChannelActor,
    /// The message body and its structure.
    pub content: ChannelContent,
    /// The replyable audience of this event, as the owning plugin resolved it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_target: Option<ChannelReplyTarget>,
    /// Structured attachments (image, file, record, voice, video).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ChannelAttachment>,
    /// The sealed raw provider payload, when the plugin persisted it. The
    /// envelope is the classified view; the raw bytes stay the evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_ref: Option<crate::object::ObjectRef>,
}

/// The sender of one inbound event, exactly as the platform describes them.
///
/// None of these fields is authority. `group_role` is a platform role inside
/// one group; `role_grant_ref` points at an owner-set administrative fact that
/// lives outside the transport.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelActor {
    /// The platform's stable account id for this actor, opaque to the core.
    pub platform_id: String,
    /// Display name the platform reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Alias the platform reported for this actor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// A person identity the contact layer already knows, when it is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_ref: Option<String>,
    /// Owner-set administrative grant for this actor. Never derived from a
    /// transport credential, a group role, a voiceprint or familiarity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_grant_ref: Option<String>,
    /// Platform group role (`owner`/`admin`/`member`), informational only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_role: Option<String>,
}

/// The body of one inbound message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelContent {
    /// The plain text the platform rendered for this message.
    pub text: String,
    /// Structured segments (text, image, file, record, mention, reply).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<ChannelSegment>,
    /// Mentioned platform ids, opaque strings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<String>,
    /// The platform id of the message this one replies to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

/// One structured part of a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelSegment {
    /// Segment kind in the plugin's own vocabulary (`text`, `image`, `file`,
    /// `record`, `mention`, `reply`, `face`, `json`).
    pub kind: String,
    /// Text carried by the segment, when it has text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The attachment this segment refers to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<String>,
    /// A platform URL for the segment, when the platform published one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// One attachment reference carried by a message.
///
/// The bytes do not travel with the message. [`ChannelAttachment::locator`]
/// names the host that owns them, so the file capability of that host can
/// read or copy them. A remote path is never handed to a local WASI mount as
/// if it were a local file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelAttachment {
    /// Stable id of this attachment inside the envelope.
    pub attachment_id: String,
    /// Attachment kind in the plugin's own vocabulary.
    pub kind: String,
    /// File name the platform reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// MIME type, when the platform reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    /// Size in bytes, when the platform reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Content hash, when the platform or the plugin computed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Where the bytes actually live.
    pub locator: AttachmentLocator,
    /// Content-addressed copy of the bytes sealed by the runtime after
    /// resolving the locator on the owning host. `None` while the locator is
    /// unresolved or the resolution failed; the locator stays authoritative
    /// for where the bytes live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<crate::object::ObjectRef>,
    /// When the locator stops being valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Where attachment bytes live.
///
/// `host` is the machine or container that owns the bytes. It is not the
/// machine this process runs on, unless the plugin says so explicitly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentLocator {
    /// Locator kind in the plugin's own vocabulary (`remote_path`, `url`,
    /// `inline`).
    pub kind: String,
    /// The host that owns the bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The path on that host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A fetchable URL, when the platform published one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// The replyable audience of one inbound event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelReplyTarget {
    /// The plugin's reply kind for this audience (`group`, `private`, ...).
    pub kind: String,
    /// The opaque platform id the model copies into the reply control.
    pub id: String,
    /// The plugin's human label for this audience, e.g. `群 12345`.
    pub label: String,
    /// The account namespace that owns the conversation.
    pub account: String,
    /// The tenant namespace, when the platform has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// The conversation identity, when it differs from `id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
}

/// The kind of outbound action a plugin describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelAction {
    /// Deliver a message.
    Send,
    /// Edit a delivered message.
    Edit,
    /// Retract a delivered message.
    Retract,
    /// Publish a typing indicator.
    Typing,
}

/// One described outbound action: what the platform must do, with which
/// parameters, and how acceptance is judged.
///
/// The plugin owns the platform action name and parameters. The durable
/// ledger owns the operation identity, the attempt and the receipt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelDispatch {
    /// The channel plugin that owns this action.
    pub channel: String,
    /// The kind of action.
    pub action: ChannelAction,
    /// The platform's own action name, e.g. `send_group_msg`.
    pub platform_action: String,
    /// The platform parameters, already scoped to the account.
    pub params: Value,
    /// The account namespace the action runs against.
    pub account: String,
    /// The tenant namespace, when the platform has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// The conversation the action addresses, when it addresses one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    /// Attachments the plugin must upload or send with this action.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ChannelAttachment>,
    /// Digest of the plugin's operation contract for this action.
    pub contract_digest: Digest,
    /// How acceptance is judged, in the plugin's own words, e.g.
    /// `onebot_retcode_zero`. Recorded with the operation; never guessed by
    /// the core.
    pub acceptance_predicate: String,
    /// The durable correlation token for this action. The runtime fills it
    /// with the operation identity before dispatch, so the provider's answer
    /// can be attributed to exactly one operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub echo: Option<String>,
}

/// The provider's answer to one dispatched action.
///
/// `status` and `retcode` keep the provider's own vocabulary. A receipt is
/// evidence that the provider answered, not that a human read the message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelReceipt {
    /// The provider's status word (`ok`, `failed`, `async`, ...).
    pub status: String,
    /// The provider's return code. The plugin defines what it means.
    pub retcode: i64,
    /// Whether the provider accepted the request, as the plugin's own
    /// acceptance predicate judges it. The core records this verdict; it
    /// never re-derives it from a status word.
    pub accepted: bool,
    /// The platform's message id, when the platform returned one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_message_id: Option<String>,
    /// The untouched provider response frame.
    pub raw: Value,
    /// When this runtime observed the answer.
    pub observed_at: chrono::DateTime<chrono::Utc>,
}

/// Why a channel operation could not be completed.
///
/// `Unknown` is the honest answer when the request may have reached the
/// platform: it must be reconciled, never blindly repeated.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChannelError {
    #[error("channel {channel} does not serve reply kind {kind}")]
    UnsupportedTarget { channel: String, kind: String },
    #[error("channel {channel} cannot resolve reply id {id}")]
    UnresolvedReply { channel: String, id: String },
    #[error("channel {channel} transport failed: {detail}")]
    Transport { channel: String, detail: String },
    #[error("channel {channel} outcome unknown: {detail}")]
    Unknown { channel: String, detail: String },
    #[error("channel {channel} refused the request: {detail}")]
    Rejected { channel: String, detail: String },
}

/// One installed channel plugin.
///
/// Implementations own their transport, their credentials and their own
/// runtime lifecycle. They never own a second operation ledger: the durable
/// intent, attempt and receipt are recorded by the shared Store path.
pub trait ChannelPlugin: Send + Sync {
    /// The stable channel id, e.g. `qq`.
    fn channel_id(&self) -> &str;

    /// The `InboxEvent.source` labels this plugin produces. At most one plugin
    /// may claim a given label.
    fn sources(&self) -> &[String];

    /// The reply audience kinds the model may address over this channel.
    fn kinds(&self) -> &[ReplyKind];

    /// Comma-joined list of replyable label nouns for the grammar line, e.g.
    /// "account or room".
    fn prompt_names(&self) -> &str;

    /// Classify one raw provider payload into a normalized envelope.
    ///
    /// Returns `None` when the payload is not a message this plugin owns.
    /// Classification never fails the ingress: an unparsable payload stays in
    /// the durable ingress ledger as raw bytes. A payload that is already a
    /// normalized envelope of this provider must classify to itself, so the
    /// model projection is identical at admission and at replay.
    fn classify(&self, source: &str, raw: &Value) -> Option<ChannelEnvelope>;

    /// Render the model's view of one inbound message: the speaker, the
    /// replyable audience and the body. The wording belongs to the plugin
    /// that owns the platform.
    fn staged_text(&self, envelope: &ChannelEnvelope) -> String;

    /// Resolve a model-facing reply id back into a sealed publication target
    /// with full account scope.
    ///
    /// The model copies the opaque id out of the staged message header, so
    /// this is the exact round trip of that id.
    fn resolve_reply(&self, kind: &str, id: &str) -> Option<PublicationTarget>;

    /// Describe the platform action for a sealed target.
    fn describe(&self, target: &PublicationTarget) -> Result<ChannelDispatch, ChannelError>;

    /// Execute one described action and return the provider's answer.
    ///
    /// The caller has already armed the durable operation, so this function
    /// only performs the external call.
    fn dispatch(
        &self,
        dispatch: &ChannelDispatch,
        content: &str,
    ) -> BoxFuture<'static, Result<ChannelReceipt, ChannelError>>;

    /// Resolve one attachment locator on the host that owns the bytes and
    /// seal the bytes into the content-addressed store, returning the
    /// attachment with `object` set. Channels bound to a host file
    /// capability override this; the default keeps the locator unresolved —
    /// the runtime then records a subject-readable `attachment_locator` gap
    /// and the message is delivered with the locator intact.
    fn resolve_attachment(
        &self,
        _attachment: &ChannelAttachment,
    ) -> Result<ChannelAttachment, String> {
        Err("attachment resolution is not available on this channel".to_string())
    }
}

/// Immutable set of registered channel plugins.
#[derive(Clone, Default)]
pub struct ChannelRegistry {
    plugins: Vec<Arc<dyn ChannelPlugin>>,
}

/// Why a channel plugin descriptor was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChannelRegistryError {
    #[error("channel id must be non-empty and match [a-zA-Z0-9._-]")]
    InvalidChannel,
    #[error("source label must be non-empty and match [a-zA-Z0-9._-]")]
    InvalidSource,
    #[error("reply tag must be non-empty and match [a-zA-Z0-9._-]")]
    InvalidTag,
    #[error("reply grammar needs non-empty id hints, audiences and names")]
    InvalidGrammar,
    #[error("duplicate channel id registered")]
    DuplicateChannel,
    #[error("duplicate inbox source claimed by two channels")]
    DuplicateSource,
    #[error("duplicate reply tag claimed by two channels")]
    DuplicateTag,
}

fn token_ok(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

impl std::fmt::Debug for ChannelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelRegistry")
            .field(
                "channels",
                &self
                    .plugins
                    .iter()
                    .map(|plugin| plugin.channel_id())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self { plugins: vec![] }
    }

    /// Validate and install one plugin. Descriptors are data: uniqueness and
    /// shape are enforced here so the model grammar cannot be ambiguous.
    pub fn register(&mut self, plugin: Arc<dyn ChannelPlugin>) -> Result<(), ChannelRegistryError> {
        if !token_ok(plugin.channel_id()) {
            return Err(ChannelRegistryError::InvalidChannel);
        }
        if !plugin.kinds().is_empty() && plugin.prompt_names().is_empty() {
            return Err(ChannelRegistryError::InvalidGrammar);
        }
        for source in plugin.sources() {
            if !token_ok(source) {
                return Err(ChannelRegistryError::InvalidSource);
            }
        }
        for kind in plugin.kinds() {
            if !token_ok(kind.tag) {
                return Err(ChannelRegistryError::InvalidTag);
            }
            if kind.id_hint.is_empty() || kind.audience.is_empty() {
                return Err(ChannelRegistryError::InvalidGrammar);
            }
        }
        if self
            .plugins
            .iter()
            .any(|p| p.channel_id() == plugin.channel_id())
        {
            return Err(ChannelRegistryError::DuplicateChannel);
        }
        for existing in &self.plugins {
            if existing.sources().iter().any(|s| plugin.sources().contains(s)) {
                return Err(ChannelRegistryError::DuplicateSource);
            }
            if existing
                .kinds()
                .iter()
                .any(|k| plugin.kinds().iter().any(|n| n.tag == k.tag))
            {
                return Err(ChannelRegistryError::DuplicateTag);
            }
        }
        self.plugins.push(plugin);
        Ok(())
    }

    /// All registered plugins, in registration order.
    pub fn plugins(&self) -> impl Iterator<Item = &Arc<dyn ChannelPlugin>> {
        self.plugins.iter()
    }

    /// Resolve a `[reply:<tag>:<id>]` audience kind to its channel.
    /// Unregistered tags stay a parse error at the model boundary.
    pub fn reply_channel(&self, tag: &str) -> Option<&str> {
        self.plugins.iter().find_map(|plugin| {
            plugin
                .kinds()
                .iter()
                .any(|kind| kind.tag == tag)
                .then_some(plugin.channel_id())
        })
    }

    /// The plugin that owns a `[reply:<tag>:<id>]` audience kind.
    pub fn plugin_for_kind(&self, tag: &str) -> Option<&Arc<dyn ChannelPlugin>> {
        self.plugins
            .iter()
            .find(|plugin| plugin.kinds().iter().any(|kind| kind.tag == tag))
    }

    /// Find the channel plugin that owns `InboxEvent.source` `source`.
    pub fn channel_for_source(&self, source: &str) -> Option<&Arc<dyn ChannelPlugin>> {
        self.plugins.iter().find(|plugin| plugin.sources().contains(&source.to_owned()))
    }

    /// Find the channel plugin with this channel id.
    pub fn channel(&self, channel: &str) -> Option<&Arc<dyn ChannelPlugin>> {
        self.plugins
            .iter()
            .find(|plugin| plugin.channel_id() == channel)
    }
}

/// Where a plugin delivers admitted inbound envelopes.
///
/// The runtime's subject admission path implements this. Returning an error
/// means "not admitted": the plugin keeps the event pending and must not
/// advance its acknowledgement.
pub trait InboundSink: Send + Sync {
    fn admit(&self, envelope: &ChannelEnvelope, raw: &Value) -> Result<(), String>;
}
