//! IM-channel plugin boundary — docs/specs/channels.md#spec-15–docs/specs/channel-adapters.md#spec-16, docs/specs/channels.md#spec-35-6, docs/specs/channels.md#spec-41-6.
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

use crate::capabilities::ChannelCapabilities;
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
    /// The platform's account-origin fact: `Some(true)` is the account this
    /// channel speaks as, `Some(false)` is an observed counterpart, and `None`
    /// means the original did not record which. In particular, historical
    /// absence is not evidence that the line came from somebody else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_account: Option<bool>,
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
    /// Provider-owned fetch descriptor, when the locator alone is not enough.
    ///
    /// A platform may hand out a media reference that needs facts the URL
    /// does not carry: a message id next to the resource key, or the symmetric
    /// key of an encrypted CDN object. Those are the platform's own facts,
    /// captured verbatim at classification time by the plugin that owns the
    /// vocabulary. Nothing outside that plugin interprets them, and they are
    /// never a location the model chose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// The most bytes one attachment resolution may deliver.
///
/// The bound belongs to the channel boundary, not to one provider: a locator
/// that names more bytes than this is refused while it is being read, instead
/// of being buffered first and rejected afterwards.
pub const MAX_ATTACHMENT_BYTES: u64 = 8 * 1024 * 1024;

/// Why a channel could not deliver an attachment's bytes.
///
/// The variants are the answers the subject can act on. They are derived from
/// what the platform actually said or did — a refused credential, an expired
/// signed locator, a missing object, a bound that was crossed — never from a
/// guess, and none of them means "the message is lost": the envelope is
/// admitted with its locator intact and the failure is a recorded fact.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttachmentError {
    /// The channel has no way to read this locator at all.
    #[error("channel {channel} does not resolve attachment {attachment_id}")]
    Unsupported {
        /// The channel id that refused.
        channel: String,
        /// The attachment id inside its envelope.
        attachment_id: String,
    },
    /// The bytes live on a host whose file capability this channel does not own.
    #[error("attachment {attachment_id} lives on host {host}; channel {channel} cannot read it")]
    ForeignHost {
        /// The channel id that refused.
        channel: String,
        /// The attachment id inside its envelope.
        attachment_id: String,
        /// The host the locator named.
        host: String,
    },
    /// The locator was refused before any provider call was made.
    #[error("attachment {attachment_id} locator refused: {detail}")]
    Refused {
        /// The attachment id inside its envelope.
        attachment_id: String,
        /// Why the plugin would not follow the locator.
        detail: String,
    },
    /// The platform's locator is no longer accepted (a signed URL that aged out).
    #[error("attachment {attachment_id} expired at the platform: {detail}")]
    Expired {
        /// The attachment id inside its envelope.
        attachment_id: String,
        /// What the platform said.
        detail: String,
    },
    /// The installed account's credential was refused for this resource.
    #[error("channel {channel} is not authenticated for attachment {attachment_id}: {detail}")]
    Unauthenticated {
        /// The channel id whose credential was refused.
        channel: String,
        /// The attachment id inside its envelope.
        attachment_id: String,
        /// What the platform said, credential-free.
        detail: String,
    },
    /// The resource is not there, or the transport could not deliver it.
    #[error("attachment {attachment_id} is unavailable: {detail}")]
    Unavailable {
        /// The attachment id inside its envelope.
        attachment_id: String,
        /// What failed, credential-free.
        detail: String,
    },
    /// The bytes exceed [`MAX_ATTACHMENT_BYTES`].
    #[error("attachment {attachment_id} exceeds the {limit}-byte channel bound")]
    TooLarge {
        /// The attachment id inside its envelope.
        attachment_id: String,
        /// The bound that was crossed.
        limit: u64,
    },
}

impl AttachmentError {
    /// A stable token for the durable channel fact that records this failure.
    pub fn token(&self) -> &'static str {
        match self {
            AttachmentError::Unsupported { .. } => "unsupported",
            AttachmentError::ForeignHost { .. } => "foreign_host",
            AttachmentError::Refused { .. } => "refused",
            AttachmentError::Expired { .. } => "expired",
            AttachmentError::Unauthenticated { .. } => "unauthenticated",
            AttachmentError::Unavailable { .. } => "unavailable",
            AttachmentError::TooLarge { .. } => "too_large",
        }
    }
}

/// One attachment's bytes, accepted in provider chunks under a hard bound.
///
/// A channel pushes the bytes as the platform delivers them; the accumulator
/// refuses the chunk that would cross [`MAX_ATTACHMENT_BYTES`], so an
/// oversized attachment costs one chunk of memory, not its whole size. The
/// assembled bytes are exactly the concatenation of the accepted chunks, in
/// arrival order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentBytes {
    attachment_id: String,
    limit: u64,
    bytes: Vec<u8>,
}

impl AttachmentBytes {
    /// Start an accumulator for one attachment.
    pub fn new(attachment_id: impl Into<String>, limit: u64) -> Self {
        Self {
            attachment_id: attachment_id.into(),
            limit,
            bytes: Vec::new(),
        }
    }

    /// The bound this accumulator enforces.
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// How many bytes were accepted so far.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether nothing was accepted yet.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Append one provider chunk.
    ///
    /// The refused chunk is not partially accepted: crossing the bound leaves
    /// the accumulator as it was and returns [`AttachmentError::TooLarge`].
    pub fn push(&mut self, chunk: &[u8]) -> Result<(), AttachmentError> {
        let next = (self.bytes.len() as u64).saturating_add(chunk.len() as u64);
        if next > self.limit {
            return Err(AttachmentError::TooLarge {
                attachment_id: self.attachment_id.clone(),
                limit: self.limit,
            });
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    /// The accepted bytes, in arrival order.
    pub fn into_vec(self) -> Vec<u8> {
        self.bytes
    }
}

/// One attachment whose bytes the owning channel delivered.
///
/// The attachment keeps its provider identity (`attachment_id`, `locator`,
/// `kind`, `name`); the resolution adds the facts it established: the MIME
/// type when the platform did not state one and the exact byte length. The
/// content-addressed `object` is filled in by whoever owns the Store, after
/// these bytes are sealed, so a plugin never needs a Store handle.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedAttachment {
    /// The attachment with the resolution's facts applied.
    pub attachment: ChannelAttachment,
    /// The platform's own bytes, bounded by [`MAX_ATTACHMENT_BYTES`].
    pub bytes: Vec<u8>,
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
    /// How many counterparts this audience reaches, as the platform event
    /// states it. `None` when the plugin cannot tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<AudienceShape>,
}

/// How many counterparts a reply audience reaches.
///
/// This is structure the platform reports, not a judgement: a private chat
/// or direct message reaches one counterpart; a group, channel or topic
/// reaches several. It is a property of the audience, not of the reply kind,
/// because one kind can name both (a Feishu chat id is a p2p chat or a group).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudienceShape {
    /// One counterpart: a private chat or direct message.
    OneToOne,
    /// Several participants: a group, channel or topic.
    Group,
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
///
/// The plugin also states what its platform can really do
/// ([`ChannelCapabilities`]) and whose account it serves
/// ([`ChannelPlugin::account_scope`]), so the runtime never has to know a
/// platform by name in order to advertise it honestly (docs/specs/channel-adapters.md#spec-15-3, docs/specs/channels.md#spec-57-4).
pub trait ChannelPlugin: Send + Sync {
    /// The stable channel id, e.g. `qq`.
    fn channel_id(&self) -> &str;

    /// The account namespace this connector's conversations belong to.
    ///
    /// The platform addresses people by its own ids, so this namespace is
    /// the plugin's own account fact — never derived from a group role or a
    /// transport token.
    fn account_scope(&self) -> String;

    /// What this platform can really do.
    ///
    /// A field that is `false` is a promise NOT to attempt the action, not a
    /// promise that the platform might accept it. The table is the plugin's
    /// own declaration: the runtime advertises exactly what a plugin says it
    /// can produce (docs/specs/channel-adapters.md#spec-15-3, docs/specs/channels.md#spec-57-4, RT19).
    fn capabilities(&self) -> ChannelCapabilities;

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

    /// Resolve one attachment locator into the platform's own bytes.
    ///
    /// The call is asynchronous because the real answer is: an authenticated
    /// request the platform serves (or a file read on the host that owns the
    /// bytes), never a value that is already in memory. Implementations read
    /// inside [`MAX_ATTACHMENT_BYTES`] and report the platform's refusal
    /// verbatim through [`AttachmentError`]; the returned attachment keeps the
    /// provider's identity and gains the facts the resolution established.
    ///
    /// Sealing the bytes into the content-addressed store is not the
    /// plugin's business: the caller owns the Store, seals the bytes and sets
    /// [`ChannelAttachment::object`]. A channel that cannot read the locator
    /// keeps this default, so the runtime records a subject-readable
    /// `attachment_locator` gap and delivers the message with the locator
    /// intact.
    fn resolve_attachment(
        &self,
        attachment: &ChannelAttachment,
    ) -> BoxFuture<'static, Result<ResolvedAttachment, AttachmentError>> {
        let channel = self.channel_id().to_string();
        let attachment_id = attachment.attachment_id.clone();
        Box::pin(async move {
            Err(AttachmentError::Unsupported {
                channel,
                attachment_id,
            })
        })
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

    /// What every registered channel can really do, in registration order.
    ///
    /// The runtime advertises this table instead of a hand-written list per
    /// platform: a channel that is installed declares its own capabilities,
    /// so a new plugin is described the same way as the shipped ones
    /// (docs/specs/channel-adapters.md#spec-15-3, docs/specs/channels.md#spec-57-4).
    pub fn capability_tables(&self) -> Vec<(String, String, ChannelCapabilities)> {
        self.plugins
            .iter()
            .map(|plugin| {
                (
                    plugin.channel_id().to_string(),
                    plugin.account_scope(),
                    plugin.capabilities(),
                )
            })
            .collect()
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

/// The replay-stable identity key of one inbound event.
///
/// It is built only from facts the provider repeats on a redelivery: the
/// transport label that carried the event, the provider, the account scope,
/// the platform message id and the declared revision. Display names, receive
/// times and local sequence numbers are deliberately excluded, so a reconnect
/// replay or an at-least-once redelivery names the same external event and the
/// control ledger's disposition check makes it idempotent.
pub fn envelope_identity_key(source: &str, envelope: &ChannelEnvelope) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        source,
        envelope.provider,
        envelope.account,
        envelope.message_id,
        envelope.source_event_revision.as_deref().unwrap_or(""),
    )
}

/// The durable event id derived from [`envelope_identity_key`].
///
/// A connector's own durable ingress ledger and the subject admission path
/// must agree on this id *before* either writes anything: the ledger stores
/// the event under it, and admission reads the same id when it asks the
/// control ledger whether a disposition already exists. Deriving it twice from
/// the same key is the same computation, never a second identity scheme.
pub fn envelope_event_id(source: &str, envelope: &ChannelEnvelope) -> crate::ids::EntityId {
    let key = envelope_identity_key(source, envelope);
    let digest = Digest::blake3(key.as_bytes());
    let hex_text = digest.0.strip_prefix("blake3:").unwrap_or(&digest.0);
    let mut bytes = [0u8; 32];
    for (index, chunk) in hex_text.as_bytes().chunks(2).take(32).enumerate() {
        let pair = std::str::from_utf8(chunk).unwrap_or("00");
        bytes[index] = u8::from_str_radix(pair, 16).unwrap_or(0);
    }
    crate::ids::EntityId::from_digest(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_bytes_accepts_within_limit() {
        let mut acc = AttachmentBytes::new("a1", 16);
        acc.push(&[1, 2, 3, 4]).unwrap();
        acc.push(&[5, 6, 7, 8]).unwrap();
        assert_eq!(acc.len(), 8);
        let vec = acc.into_vec();
        assert_eq!(vec, &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn attachment_bytes_rejects_at_limit() {
        let mut acc = AttachmentBytes::new("a2", 4);
        acc.push(&[1, 2, 3, 4]).unwrap();
        let err = acc.push(&[5]).unwrap_err();
        assert!(
            matches!(err, AttachmentError::TooLarge { limit: 4, .. }),
            "expected TooLarge, got {err:?}"
        );
        // The accumulator is unchanged after rejection.
        assert_eq!(acc.len(), 4);
    }

    #[test]
    fn attachment_bytes_rejects_oversized_first_chunk() {
        let mut acc = AttachmentBytes::new("a3", 2);
        let err = acc.push(&[1, 2, 3]).unwrap_err();
        assert!(matches!(err, AttachmentError::TooLarge { .. }));
        assert!(acc.is_empty());
    }

    #[test]
    fn attachment_bytes_zero_limit() {
        let mut acc = AttachmentBytes::new("a4", 0);
        let err = acc.push(&[1]).unwrap_err();
        assert!(matches!(err, AttachmentError::TooLarge { limit: 0, .. }));
        assert!(acc.is_empty());
    }

    #[test]
    fn attachment_bytes_empty_chunks_are_fine() {
        let mut acc = AttachmentBytes::new("a5", 4);
        acc.push(&[]).unwrap();
        acc.push(&[]).unwrap();
        assert!(acc.is_empty());
    }

    #[test]
    fn max_attachment_bytes_is_eight_mib() {
        assert_eq!(MAX_ATTACHMENT_BYTES, 8 * 1024 * 1024);
    }
}
