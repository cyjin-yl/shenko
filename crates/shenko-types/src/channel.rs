//! IM-channel plugin boundary.
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
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::capabilities::ChannelCapabilities;
use crate::generation::{
    GenerationIdentityError, GenerationRef, PreparedAttachmentError, PreparedRole,
    SealedGenerationBinding,
};
use crate::object::{Digest, VersionedRef};
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
    /// The plugin that produced this envelope is no longer the installed
    /// generation's implementation of this channel.
    #[error("channel {channel} no longer owns attachment {attachment_id}: {detail}")]
    GenerationRefused {
        /// The channel id whose installed generation was displaced.
        channel: String,
        /// The attachment id inside its envelope.
        attachment_id: String,
        /// Why the binding was refused.
        detail: String,
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
            AttachmentError::GenerationRefused { .. } => "generation_refused",
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
    /// The action was refused before the platform was contacted because the
    /// plugin is not the installed generation's implementation of the channel.
    /// This is a deterministic refusal (effect none), never an unknown
    /// outcome: nothing was sent.
    #[error("channel {channel} refused the request from a displaced generation: {detail}")]
    GenerationRefused { channel: String, detail: String },
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
/// platform by name in order to advertise it honestly.
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
    /// can produce (RT19).
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

/// One channel plugin exactly as an installed prepared generation publishes it.
///
/// The binding is the plugin's own prepared-generation identity
/// ([`PreparedRole::Channel`]). A plugin paired with a binding that names some
/// other role, or with a malformed identity, is refused at construction rather
/// than admitted with a looser identity.
#[derive(Clone)]
pub struct PreparedChannel {
    binding: SealedGenerationBinding,
    plugin: Arc<dyn ChannelPlugin>,
}

impl PreparedChannel {
    /// Pair one live plugin with the exact binding that names it.
    pub fn new(
        binding: SealedGenerationBinding,
        plugin: Arc<dyn ChannelPlugin>,
    ) -> Result<Self, ChannelBindingError> {
        let channel = plugin.channel_id().to_string();
        binding
            .validate()
            .map_err(|source| ChannelBindingError::InvalidBinding {
                channel: channel.clone(),
                source,
            })?;
        if binding.role != PreparedRole::Channel {
            return Err(ChannelBindingError::WrongRole {
                channel,
                role: binding.role,
            });
        }
        Ok(Self { binding, plugin })
    }

    /// The exact prepared-generation identity of this channel role.
    pub fn binding(&self) -> &SealedGenerationBinding {
        &self.binding
    }

    pub fn channel_id(&self) -> &str {
        self.plugin.channel_id()
    }

    pub fn sources(&self) -> &[String] {
        self.plugin.sources()
    }

    pub fn plugin(&self) -> &Arc<dyn ChannelPlugin> {
        &self.plugin
    }
}

impl std::fmt::Debug for PreparedChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedChannel")
            .field("channel", &self.plugin.channel_id())
            .field("binding", &self.binding)
            .finish()
    }
}

/// The channel plugins of one prepared generation.
///
/// This is the channel read path once a generation is installed. Preparation
/// builds the live plugins outside any durable transaction; the source names
/// exactly the generation it publishes, and [`Self::channel_for_binding`] never
/// substitutes a successor, a compiled shadow or another channel's
/// implementation.
///
/// The source is the generation owner's own authority: a successor is published
/// by that owner swapping the catalog this source returns, never by attaching a
/// second source to the registry.
pub trait PreparedChannelSource: Send + Sync + 'static {
    /// The generation this source publishes, when one is published.
    fn generation_ref(&self) -> Option<GenerationRef>;

    /// The exact channels of the currently running generation.
    fn current_channels(&self) -> Vec<PreparedChannel>;

    /// Resolve exactly one prepared binding this generation publishes.
    fn channel_for_binding(
        &self,
        binding: &SealedGenerationBinding,
    ) -> Option<Arc<dyn ChannelPlugin>>;
}

/// A discovered channel handle bound to the installed generation.
///
/// Discovery returns this handle instead of a bare plugin: the plugin facts
/// (id, sources, reply kinds, labels) are reads of the declared contract, while
/// every operation that classifies, renders, resolves or performs a platform
/// action first proves the handle still belongs to the installed generation.
///
/// A handle from a displaced generation is refused: it may not classify a new
/// payload, resolve a reply, describe or dispatch an action, or read an
/// attachment. A plugin instance the installed generation no longer publishes
/// is refused the same way even when its metadata is identical.
#[derive(Clone)]
pub struct InstalledChannel {
    state: Arc<ChannelRegistryState>,
    plugin: Arc<dyn ChannelPlugin>,
    binding: Option<SealedGenerationBinding>,
    generation: Option<GenerationRef>,
}

impl InstalledChannel {
    pub fn channel_id(&self) -> &str {
        self.plugin.channel_id()
    }

    pub fn account_scope(&self) -> String {
        self.plugin.account_scope()
    }

    pub fn sources(&self) -> &[String] {
        self.plugin.sources()
    }

    pub fn kinds(&self) -> &[ReplyKind] {
        self.plugin.kinds()
    }

    pub fn prompt_names(&self) -> &str {
        self.plugin.prompt_names()
    }

    /// The exact prepared binding of this handle, when it was discovered from
    /// an attached prepared generation.
    pub fn binding(&self) -> Option<&SealedGenerationBinding> {
        self.binding.as_ref()
    }

    /// The generation this handle was discovered under, when it was prepared.
    pub fn generation(&self) -> Option<&GenerationRef> {
        self.generation.as_ref()
    }

    /// The live plugin allocation. Holding it grants no authority: every
    /// operation through this handle still proves the installed generation.
    pub fn plugin(&self) -> &Arc<dyn ChannelPlugin> {
        &self.plugin
    }

    /// Prove this handle is still the installed generation's implementation.
    ///
    /// A handle discovered without a prepared generation is a compiled channel
    /// of this process and is current by construction. Once a prepared
    /// generation is installed, only channels that generation publishes (with
    /// the exact same binding and the same live allocation) are current.
    pub fn ensure_bound(&self) -> Result<(), ChannelBindingError> {
        let channel = self.plugin.channel_id().to_string();
        let prepared = read_lock(&self.state.prepared).clone();
        let Some(source) = prepared else {
            return if self.binding.is_some() {
                Err(ChannelBindingError::NoInstalledGeneration { channel })
            } else {
                Ok(())
            };
        };
        let installed = source
            .generation_ref()
            .ok_or_else(|| ChannelBindingError::NoInstalledGeneration {
                channel: channel.clone(),
            })?;
        let Some(binding) = self.binding.as_ref() else {
            // A compiled plugin is not part of the prepared generation; it is
            // never served alongside one, even as a fallback.
            return Err(ChannelBindingError::ForeignPlugin { channel });
        };
        binding.validate().map_err(|source| {
            ChannelBindingError::InvalidBinding {
                channel: channel.clone(),
                source,
            }
        })?;
        if binding.role != PreparedRole::Channel {
            return Err(ChannelBindingError::WrongRole {
                channel,
                role: binding.role,
            });
        }
        if let Some(bound) = self.generation.as_ref()
            && !installed.same_content(bound)
        {
            return Err(ChannelBindingError::StaleGeneration {
                channel,
                bound: bound.clone(),
                installed: Some(installed),
            });
        }
        let live = source
            .channel_for_binding(binding)
            .ok_or_else(|| ChannelBindingError::RetiredBinding {
                channel: channel.clone(),
            })?;
        if !Arc::ptr_eq(&live, &self.plugin) {
            return Err(ChannelBindingError::ForeignPlugin { channel });
        }
        Ok(())
    }

    /// What this channel can really do, read only from the running generation.
    pub fn capabilities_current(&self) -> Result<ChannelCapabilities, ChannelBindingError> {
        self.ensure_bound()?;
        Ok(self.plugin.capabilities())
    }

    /// Classify one inbound payload through the running generation.
    pub fn classify(
        &self,
        source: &str,
        raw: &Value,
    ) -> Result<Option<ChannelEnvelope>, ChannelBindingError> {
        self.ensure_bound()?;
        Ok(self.plugin.classify(source, raw))
    }

    /// Render the model's view of one inbound message through the running
    /// generation.
    pub fn staged_text(&self, envelope: &ChannelEnvelope) -> Result<String, ChannelBindingError> {
        self.ensure_bound()?;
        Ok(self.plugin.staged_text(envelope))
    }

    /// Resolve a model-facing reply id through the running generation.
    pub fn resolve_reply(
        &self,
        kind: &str,
        id: &str,
    ) -> Result<Option<PublicationTarget>, ChannelBindingError> {
        self.ensure_bound()?;
        Ok(self.plugin.resolve_reply(kind, id))
    }

    /// Describe one sealed target. A displaced generation is a deterministic
    /// refusal before any platform action exists.
    pub fn describe(&self, target: &PublicationTarget) -> Result<ChannelDispatch, ChannelError> {
        self.ensure_bound()
            .map_err(|error| ChannelError::GenerationRefused {
                channel: self.plugin.channel_id().to_string(),
                detail: error.to_string(),
            })?;
        self.plugin.describe(target)
    }

    /// Execute one described action. The generation is proven before the
    /// provider future is created, so a displaced handle never reaches the
    /// network.
    pub fn dispatch(
        &self,
        dispatch: &ChannelDispatch,
        content: &str,
    ) -> BoxFuture<'static, Result<ChannelReceipt, ChannelError>> {
        match self.ensure_bound() {
            Ok(()) => self.plugin.dispatch(dispatch, content),
            Err(error) => {
                let channel = self.plugin.channel_id().to_string();
                let detail = error.to_string();
                Box::pin(async move { Err(ChannelError::GenerationRefused { channel, detail }) })
            }
        }
    }

    /// Resolve one attachment through the running generation. A displaced
    /// handle refuses before the owning host is contacted.
    pub fn resolve_attachment(
        &self,
        attachment: &ChannelAttachment,
    ) -> BoxFuture<'static, Result<ResolvedAttachment, AttachmentError>> {
        match self.ensure_bound() {
            Ok(()) => self.plugin.resolve_attachment(attachment),
            Err(error) => {
                let channel = self.plugin.channel_id().to_string();
                let attachment_id = attachment.attachment_id.clone();
                let detail = error.to_string();
                Box::pin(async move {
                    Err(AttachmentError::GenerationRefused {
                        channel,
                        attachment_id,
                        detail,
                    })
                })
            }
        }
    }
}

impl std::fmt::Debug for InstalledChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstalledChannel")
            .field("channel", &self.plugin.channel_id())
            .field("binding", &self.binding)
            .finish()
    }
}

/// Why a discovered channel handle is not the installed generation's role.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChannelBindingError {
    #[error("channel {channel} binding does not describe a channel role (found {role:?})")]
    WrongRole {
        channel: String,
        role: PreparedRole,
    },
    #[error("channel {channel} binding is malformed: {source}")]
    InvalidBinding {
        channel: String,
        #[source]
        source: GenerationIdentityError,
    },
    #[error("channel {channel} descriptor cannot be encoded: {detail}")]
    Descriptor { channel: String, detail: String },
    #[error("channel {channel} was sealed for generation {bound}, but the installed generation is {installed:?}")]
    StaleGeneration {
        channel: String,
        bound: GenerationRef,
        installed: Option<GenerationRef>,
    },
    #[error("channel {channel} has no published installed generation")]
    NoInstalledGeneration { channel: String },
    #[error("channel {channel} is no longer published by the installed generation")]
    RetiredBinding { channel: String },
    #[error("channel {channel} is not the implementation the installed generation publishes")]
    ForeignPlugin { channel: String },
    #[error("the installed channel generation cannot be attached: {0}")]
    Attachment(#[from] PreparedAttachmentError),
    #[error(transparent)]
    Registry(#[from] ChannelRegistryError),
}

/// The shared live state of one channel installation.
///
/// Clones of [`ChannelRegistry`] are views of this state: a generation
/// installed or displaced once is seen by every existing view, including the
/// ingress loops and the publication consumer that already hold a clone.
struct ChannelRegistryState {
    compiled: RwLock<Vec<Arc<dyn ChannelPlugin>>>,
    prepared: RwLock<Option<Arc<dyn PreparedChannelSource>>>,
}

/// The installed channel generation and the compiled plugins it was sealed from.
///
/// Registration is the composition root's install path; it is only possible
/// before a prepared generation is attached. Discovery and every message
/// operation then resolve through that one generation.
pub struct ChannelRegistry {
    state: Arc<ChannelRegistryState>,
}

impl Clone for ChannelRegistry {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

impl Default for ChannelRegistry {
    fn default() -> Self {
        Self::new()
    }
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
    #[error("a prepared channel generation is already installed; register before attaching it")]
    GenerationInstalled,
}

fn token_ok(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// A lock whose poison no longer matters: the guarded value is plain registry
/// state, so the last complete write stays readable after a panicking reader.
fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poison| poison.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|poison| poison.into_inner())
}

/// Validate the advertised contract of one plugin.
///
/// Compiled registration and prepared catalogs share this exact admission: the
/// model grammar cannot become ambiguous merely because the plugins arrived
/// through a sealed generation instead of a direct call.
fn validate_channel_descriptor(
    plugin: &Arc<dyn ChannelPlugin>,
) -> Result<(), ChannelRegistryError> {
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
    Ok(())
}

/// Uniqueness of one prepared catalog's channel ids, sources and reply tags.
fn validate_channel_set(channels: &[PreparedChannel]) -> Result<(), ChannelBindingError> {
    let mut ids = std::collections::HashSet::new();
    let mut sources = std::collections::HashSet::new();
    let mut tags = std::collections::HashSet::new();
    for channel in channels {
        validate_channel_descriptor(channel.plugin())?;
        if !ids.insert(channel.channel_id().to_string()) {
            return Err(ChannelRegistryError::DuplicateChannel.into());
        }
        for label in channel.sources() {
            if !sources.insert(label.clone()) {
                return Err(ChannelRegistryError::DuplicateSource.into());
            }
        }
        for kind in channel.plugin().kinds() {
            if !tags.insert(kind.tag) {
                return Err(ChannelRegistryError::DuplicateTag.into());
            }
        }
    }
    Ok(())
}

/// Validate the catalog a source currently publishes before it becomes the
/// installed generation.
fn validate_prepared_catalog(
    source: &Arc<dyn PreparedChannelSource>,
) -> Result<(), ChannelBindingError> {
    validate_channel_set(&source.current_channels())
}

impl std::fmt::Debug for ChannelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelRegistry")
            .field("generation", &self.installed_generation())
            .field("channels", &self.plugins().iter().map(InstalledChannel::channel_id).collect::<Vec<_>>())
            .finish()
    }
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self {
            state: Arc::new(ChannelRegistryState {
                compiled: RwLock::new(Vec::new()),
                prepared: RwLock::new(None),
            }),
        }
    }

    /// The compiled plugins this process registered, in registration order.
    ///
    /// This is the material a composition root seals into a prepared
    /// generation. Reading it grants no execution authority: operations go
    /// through [`Self::plugins`] and the installed generation.
    pub fn compiled_plugins(&self) -> Vec<Arc<dyn ChannelPlugin>> {
        read_lock(&self.state.compiled).clone()
    }

    /// The prepared generation whose channel catalog is authoritative.
    pub fn installed_generation(&self) -> Option<GenerationRef> {
        read_lock(&self.state.prepared)
            .as_ref()
            .and_then(|source| source.generation_ref())
    }

    /// Attach the prepared generation's channel catalog.
    ///
    /// Single-shot: a second attachment is refused, so a live process cannot
    /// hold two authoritative channel generations. A successor is published by
    /// the generation owner's one swap, which reaches this source, not this
    /// method.
    pub fn attach_prepared_source(
        &mut self,
        source: Arc<dyn PreparedChannelSource>,
    ) -> Result<(), ChannelBindingError> {
        self.prepare_prepared_source_attachment(source)?.commit();
        Ok(())
    }

    /// Check exact source identity under the live attachment lock without
    /// changing it. The same allocation is idempotent; identical generation
    /// metadata from another source cannot replace the installed port.
    pub fn prepare_prepared_source_attachment(
        &mut self,
        source: Arc<dyn PreparedChannelSource>,
    ) -> Result<PreparedChannelAttachment<'_>, ChannelBindingError> {
        let prepared = write_lock(&self.state.prepared);
        let incoming = source.generation_ref();
        let same = prepared
            .as_ref()
            .is_some_and(|attached| Arc::ptr_eq(attached, &source));
        PreparedAttachmentError::check(
            same,
            prepared
                .as_ref()
                .and_then(|attached| attached.generation_ref()),
            incoming,
        )?;
        if !same {
            validate_prepared_catalog(&source)?;
        }
        Ok(PreparedChannelAttachment {
            prepared,
            incoming: Some(source),
        })
    }

    /// Validate and install one compiled plugin.
    ///
    /// Descriptors are data: uniqueness and shape are enforced here so the
    /// model grammar cannot be ambiguous. Registration is the install path and
    /// is refused once a prepared generation is attached — a plugin a sealed
    /// generation does not publish cannot be introduced behind its back.
    pub fn register(&mut self, plugin: Arc<dyn ChannelPlugin>) -> Result<(), ChannelRegistryError> {
        if read_lock(&self.state.prepared).is_some() {
            return Err(ChannelRegistryError::GenerationInstalled);
        }
        validate_channel_descriptor(&plugin)?;
        let mut compiled = write_lock(&self.state.compiled);
        if read_lock(&self.state.prepared).is_some() {
            return Err(ChannelRegistryError::GenerationInstalled);
        }
        if compiled
            .iter()
            .any(|existing| existing.channel_id() == plugin.channel_id())
        {
            return Err(ChannelRegistryError::DuplicateChannel);
        }
        for existing in compiled.iter() {
            if existing
                .sources()
                .iter()
                .any(|source| plugin.sources().contains(source))
            {
                return Err(ChannelRegistryError::DuplicateSource);
            }
            if existing
                .kinds()
                .iter()
                .any(|known| plugin.kinds().iter().any(|kind| kind.tag == known.tag))
            {
                return Err(ChannelRegistryError::DuplicateTag);
            }
        }
        compiled.push(plugin);
        Ok(())
    }

    /// The channels of the installed generation, in catalog order.
    ///
    /// With a prepared generation this is exactly its current catalog; without
    /// one it is the compiled set this process installed. A compiled plugin is
    /// never mixed into a prepared generation, not even as a fallback.
    pub fn plugins(&self) -> Vec<InstalledChannel> {
        let prepared = read_lock(&self.state.prepared).clone();
        match prepared {
            Some(source) => {
                let generation = source.generation_ref();
                source
                    .current_channels()
                    .into_iter()
                    .map(|channel| InstalledChannel {
                        state: Arc::clone(&self.state),
                        plugin: Arc::clone(channel.plugin()),
                        binding: Some(channel.binding().clone()),
                        generation: generation.clone(),
                    })
                    .collect()
            }
            None => read_lock(&self.state.compiled)
                .iter()
                .map(|plugin| InstalledChannel {
                    state: Arc::clone(&self.state),
                    plugin: Arc::clone(plugin),
                    binding: None,
                    generation: None,
                })
                .collect(),
        }
    }

    /// What every installed channel can really do, read from the running
    /// generation only.
    ///
    /// The runtime advertises this table instead of a hand-written list per
    /// platform: a channel that is installed declares its own capabilities,
    /// so a new plugin is described the same way as the shipped ones. A
    /// channel whose handle is no longer current fails the whole table instead
    /// of advertising a displaced declaration.
    pub fn capability_tables(
        &self,
    ) -> Result<Vec<(String, String, ChannelCapabilities)>, ChannelBindingError> {
        self.plugins()
            .into_iter()
            .map(|channel| {
                let capabilities = channel.capabilities_current()?;
                Ok((
                    channel.channel_id().to_string(),
                    channel.account_scope(),
                    capabilities,
                ))
            })
            .collect()
    }

    /// Resolve a `[reply:<tag>:<id>]` audience kind to its channel.
    /// Unregistered tags stay a parse error at the model boundary.
    pub fn reply_channel(&self, tag: &str) -> Option<String> {
        self.plugin_for_kind(tag)
            .map(|channel| channel.channel_id().to_string())
    }

    /// The channel handle that owns a `[reply:<tag>:<id>]` audience kind.
    pub fn plugin_for_kind(&self, tag: &str) -> Option<InstalledChannel> {
        self.plugins()
            .into_iter()
            .find(|channel| channel.kinds().iter().any(|kind| kind.tag == tag))
    }

    /// Find the channel handle that owns `InboxEvent.source` `source`.
    pub fn channel_for_source(&self, source: &str) -> Option<InstalledChannel> {
        self.plugins()
            .into_iter()
            .find(|channel| channel.sources().iter().any(|label| label.as_str() == source))
    }

    /// Find the channel handle with this channel id.
    pub fn channel(&self, channel: &str) -> Option<InstalledChannel> {
        self.plugins()
            .into_iter()
            .find(|installed| installed.channel_id() == channel)
    }
}

/// A checked channel-generation attachment holding the actual write lock.
///
/// A coordinator preflights the port, then retains this guard through the
/// pointer store. Registration cannot slip in between the two.
pub struct PreparedChannelAttachment<'a> {
    prepared: RwLockWriteGuard<'a, Option<Arc<dyn PreparedChannelSource>>>,
    incoming: Option<Arc<dyn PreparedChannelSource>>,
}

impl PreparedChannelAttachment<'_> {
    /// Infallible pointer store, retaining the lock until this guard is
    /// dropped.
    pub fn commit(&mut self) {
        if self.prepared.is_none() {
            *self.prepared = self.incoming.take();
        }
    }
}

/// One immutable prepared channel generation, ready to attach.
pub struct PreparedChannelCatalog {
    generation: GenerationRef,
    channels: Vec<PreparedChannel>,
}

impl PreparedChannelCatalog {
    /// Admit a generation and its exact channel roles.
    pub fn new(
        generation: GenerationRef,
        channels: Vec<PreparedChannel>,
    ) -> Result<Self, ChannelBindingError> {
        generation
            .validate()
            .map_err(|source| ChannelBindingError::InvalidBinding {
                channel: "generation".into(),
                source,
            })?;
        validate_channel_set(&channels)?;
        Ok(Self {
            generation,
            channels,
        })
    }

    pub fn generation(&self) -> &GenerationRef {
        &self.generation
    }

    pub fn channels(&self) -> &[PreparedChannel] {
        &self.channels
    }
}

impl PreparedChannelSource for PreparedChannelCatalog {
    fn generation_ref(&self) -> Option<GenerationRef> {
        Some(self.generation.clone())
    }

    fn current_channels(&self) -> Vec<PreparedChannel> {
        self.channels.clone()
    }

    fn channel_for_binding(
        &self,
        binding: &SealedGenerationBinding,
    ) -> Option<Arc<dyn ChannelPlugin>> {
        self.channels
            .iter()
            .find(|channel| channel.binding().matches_exact(binding))
            .map(|channel| Arc::clone(channel.plugin()))
    }
}

impl std::fmt::Debug for PreparedChannelCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedChannelCatalog")
            .field("generation", &self.generation)
            .field("channels", &self.channels)
            .finish()
    }
}

/// The exact declared contract of one channel role.
///
/// The descriptor is what a prepared binding captures: the channel id, the
/// account namespace, the inbox sources, the reply grammar and the capability
/// table. A changed declaration therefore names a different role even when the
/// executable and the transport metadata are unchanged.
pub fn channel_descriptor_digest(
    plugin: &Arc<dyn ChannelPlugin>,
) -> Result<Digest, ChannelBindingError> {
    let kinds: Vec<Value> = plugin
        .kinds()
        .iter()
        .map(|kind| {
            serde_json::json!({
                "tag": kind.tag,
                "id_hint": kind.id_hint,
                "audience": kind.audience,
            })
        })
        .collect();
    let descriptor = serde_json::json!({
        "schema": "shenko.channel-descriptor/1",
        "channel_id": plugin.channel_id(),
        "account_scope": plugin.account_scope(),
        "sources": plugin.sources(),
        "kinds": kinds,
        "prompt_names": plugin.prompt_names(),
        "capabilities": plugin.capabilities(),
    });
    let bytes = serde_json::to_vec(&descriptor).map_err(|error| {
        ChannelBindingError::Descriptor {
            channel: plugin.channel_id().to_string(),
            detail: error.to_string(),
        }
    })?;
    Ok(Digest::blake3(&bytes))
}

/// Bind the compiled channel plugins a composition root installed as one
/// prepared generation.
///
/// `executable` is the artifact that actually contains the compiled plugins
/// (the running executable's own bytes). Each role's descriptor is the
/// declared channel contract, and the generation digest is derived from the
/// executable plus the ordered role bindings. Two processes running the same
/// binary with the same declarations therefore name the same generation
/// content; any changed declaration names a different one. `seq` is this
/// process's publication sequence and is diagnostic only.
pub fn prepare_compiled_channels(
    seq: u64,
    executable: Digest,
    plugins: Vec<Arc<dyn ChannelPlugin>>,
) -> Result<PreparedChannelCatalog, ChannelBindingError> {
    let artifact = crate::object::ArtifactRef {
        digest: executable.clone(),
        locator: format!("native:executable/{executable}"),
    };
    let schema = crate::object::SchemaRef {
        name: "shenko.channel-plugin".into(),
        major: 1,
        digest: Digest::blake3(b"shenko.channel-plugin/1"),
    };
    let mut channels = Vec::with_capacity(plugins.len());
    let mut roles = Vec::with_capacity(plugins.len());
    for plugin in plugins {
        let descriptor = channel_descriptor_digest(&plugin)?;
        let implementation = VersionedRef {
            artifact: artifact.clone(),
            symbol: format!("shenko.channel/{}", plugin.channel_id()),
            schema: schema.clone(),
        };
        let binding = SealedGenerationBinding::compiled(
            PreparedRole::Channel,
            implementation,
            descriptor.clone(),
            executable.clone(),
        );
        roles.push(serde_json::json!({
            "channel": plugin.channel_id(),
            "sources": plugin.sources(),
            "binding": &binding,
            "descriptor": descriptor,
        }));
        channels.push(PreparedChannel::new(binding, plugin)?);
    }
    // Discovery keeps installation order; the generation identity is
    // order-independent so a differently ordered install still names the same
    // catalog.
    roles.sort_by(|left, right| {
        left.get("channel")
            .and_then(Value::as_str)
            .cmp(&right.get("channel").and_then(Value::as_str))
    });
    let catalog = serde_json::json!({
        "schema": "shenko.channel-generation/1",
        "executable": executable,
        "channels": roles,
    });
    let bytes = serde_json::to_vec(&catalog).map_err(|error| ChannelBindingError::Descriptor {
        channel: "generation".into(),
        detail: error.to_string(),
    })?;
    PreparedChannelCatalog::new(GenerationRef::new(seq, Digest::blake3(&bytes)), channels)
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

    // ----- installed-generation binding -----------------------------------

    use crate::object::{ArtifactRef, SchemaRef};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct StubChannel {
        channel: &'static str,
        sources: Vec<String>,
        kinds: Vec<ReplyKind>,
        prompt_names: &'static str,
        notes: &'static str,
        dispatches: Arc<AtomicUsize>,
    }

    impl StubChannel {
        fn new(
            channel: &'static str,
            source: &'static str,
            tag: &'static str,
            notes: &'static str,
        ) -> Self {
            Self {
                channel,
                sources: vec![source.to_string()],
                kinds: vec![ReplyKind {
                    tag,
                    id_hint: "房间号",
                    audience: "房间",
                }],
                prompt_names: "房间号",
                notes,
                dispatches: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn dispatches(&self) -> usize {
            self.dispatches.load(Ordering::SeqCst)
        }
    }

    impl ChannelPlugin for StubChannel {
        fn channel_id(&self) -> &str {
            self.channel
        }

        fn account_scope(&self) -> String {
            format!("stub:{}", self.channel)
        }

        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities {
                direct: true,
                group: false,
                thread: false,
                guild_channel: false,
                rich_text: false,
                attachments: false,
                edit: false,
                retract: false,
                typing: false,
                proactive: false,
                notes: vec![self.notes.to_string()],
            }
        }

        fn sources(&self) -> &[String] {
            &self.sources
        }

        fn kinds(&self) -> &[ReplyKind] {
            &self.kinds
        }

        fn prompt_names(&self) -> &str {
            self.prompt_names
        }

        fn classify(&self, _source: &str, _raw: &Value) -> Option<ChannelEnvelope> {
            None
        }

        fn staged_text(&self, envelope: &ChannelEnvelope) -> String {
            format!("[{}] {}", self.channel, envelope.message_id)
        }

        fn resolve_reply(&self, kind: &str, id: &str) -> Option<PublicationTarget> {
            Some(PublicationTarget::new(
                self.channel,
                kind,
                id,
                self.account_scope(),
            ))
        }

        fn describe(&self, target: &PublicationTarget) -> Result<ChannelDispatch, ChannelError> {
            Ok(ChannelDispatch {
                channel: self.channel.to_string(),
                action: ChannelAction::Send,
                platform_action: "stub_send".into(),
                params: serde_json::json!({"to": target.id}),
                account: self.account_scope(),
                tenant: None,
                conversation: None,
                attachments: Vec::new(),
                contract_digest: Digest::blake3(b"stub-contract"),
                acceptance_predicate: "stub_ok".into(),
                echo: None,
            })
        }

        fn dispatch(
            &self,
            _dispatch: &ChannelDispatch,
            _content: &str,
        ) -> BoxFuture<'static, Result<ChannelReceipt, ChannelError>> {
            self.dispatches.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(ChannelReceipt {
                    status: "ok".into(),
                    retcode: 0,
                    accepted: true,
                    provider_message_id: None,
                    raw: serde_json::json!({}),
                    observed_at: chrono::Utc::now(),
                })
            })
        }
    }

    fn channel_binding(
        role: PreparedRole,
        artifact: &[u8],
        descriptor: &[u8],
    ) -> SealedGenerationBinding {
        let digest = Digest::blake3(artifact);
        SealedGenerationBinding::compiled(
            role,
            VersionedRef {
                artifact: ArtifactRef {
                    digest: digest.clone(),
                    locator: "native:test".into(),
                },
                symbol: "shenko.channel/test".into(),
                schema: SchemaRef {
                    name: "shenko.channel-plugin".into(),
                    major: 1,
                    digest,
                },
            },
            Digest::blake3(descriptor),
            Digest::blake3(b"closure"),
        )
    }

    fn prepared(
        plugin: &Arc<StubChannel>,
        artifact: &[u8],
        descriptor: &[u8],
    ) -> PreparedChannel {
        PreparedChannel::new(
            channel_binding(PreparedRole::Channel, artifact, descriptor),
            plugin.clone(),
        )
        .unwrap()
    }

    fn target() -> PublicationTarget {
        PublicationTarget::new("stub", "stub-tag", "9", "stub:stub")
    }

    /// A source whose published generation and catalog can advance, standing
    /// in for the generation owner's one pointer swap.
    struct MutableSource {
        state: Mutex<(Option<GenerationRef>, Vec<PreparedChannel>)>,
    }

    impl MutableSource {
        fn new() -> Self {
            Self {
                state: Mutex::new((None, Vec::new())),
            }
        }

        fn install(&self, generation: GenerationRef, channels: Vec<PreparedChannel>) {
            *self.state.lock().unwrap() = (Some(generation), channels);
        }
    }

    impl PreparedChannelSource for MutableSource {
        fn generation_ref(&self) -> Option<GenerationRef> {
            self.state.lock().unwrap().0.clone()
        }

        fn current_channels(&self) -> Vec<PreparedChannel> {
            self.state.lock().unwrap().1.clone()
        }

        fn channel_for_binding(
            &self,
            binding: &SealedGenerationBinding,
        ) -> Option<Arc<dyn ChannelPlugin>> {
            self.state
                .lock()
                .unwrap()
                .1
                .iter()
                .find(|channel| channel.binding().matches_exact(binding))
                .map(|channel| Arc::clone(channel.plugin()))
        }
    }

    /// A prepared generation owns discovery: a compiled plugin with the same
    /// channel id is retained as install material but never served, and a late
    /// compiled registration is refused.
    #[test]
    fn an_installed_generation_shadows_compiled_plugins_and_closes_registration() {
        let mut registry = ChannelRegistry::new();
        let compiled = Arc::new(StubChannel::new("stub", "stub-src", "stub-tag", "compiled"));
        registry.register(compiled.clone()).unwrap();
        let prepared_plugin = Arc::new(StubChannel::new("stub", "stub-src", "stub-tag", "prepared"));
        let generation = GenerationRef::new(1, Digest::blake3(b"catalog-1"));
        let catalog = PreparedChannelCatalog::new(
            generation.clone(),
            vec![prepared(&prepared_plugin, b"artifact", b"declared")],
        )
        .unwrap();
        let prepared_handle: Arc<dyn ChannelPlugin> = prepared_plugin.clone();
        let compiled_handle = registry.plugins().remove(0);
        registry.attach_prepared_source(Arc::new(catalog)).unwrap();

        assert_eq!(registry.compiled_plugins().len(), 1);
        assert!(
            registry
                .installed_generation()
                .unwrap()
                .same_content(&generation)
        );
        let discovered = registry.plugins();
        assert_eq!(discovered.len(), 1);
        assert!(Arc::ptr_eq(discovered[0].plugin(), &prepared_handle));
        assert!(discovered[0].ensure_bound().is_ok());
        // The pre-attachment compiled handle is not part of the generation.
        assert!(matches!(
            compiled_handle.ensure_bound(),
            Err(ChannelBindingError::ForeignPlugin { .. })
        ));
        // Installing another plugin behind the installed generation is refused.
        assert!(matches!(
            registry.register(Arc::new(StubChannel::new(
                "late",
                "late-src",
                "late-tag",
                "late"
            ))),
            Err(ChannelRegistryError::GenerationInstalled)
        ));
    }

    /// A displaced generation refuses every operation before the platform is
    /// contacted, while discovery and the capability table move to the new
    /// generation.
    #[test]
    fn a_displaced_generation_refuses_execution_and_capabilities() {
        let mut registry = ChannelRegistry::new();
        let source = Arc::new(MutableSource::new());
        let first = Arc::new(StubChannel::new("stub", "stub-src", "stub-tag", "first"));
        let first_generation = GenerationRef::new(1, Digest::blake3(b"gen-1"));
        source.install(
            first_generation.clone(),
            vec![prepared(&first, b"artifact-1", b"descriptor-1")],
        );
        registry.attach_prepared_source(source.clone()).unwrap();
        let handle = registry.plugin_for_kind("stub-tag").unwrap();
        assert!(handle.ensure_bound().is_ok());
        assert_eq!(registry.capability_tables().unwrap().len(), 1);

        let second = Arc::new(StubChannel::new("stub", "stub-src", "stub-tag", "second"));
        let second_generation = GenerationRef::new(2, Digest::blake3(b"gen-2"));
        source.install(
            second_generation.clone(),
            vec![prepared(&second, b"artifact-2", b"descriptor-2")],
        );

        assert!(matches!(
            handle.ensure_bound(),
            Err(ChannelBindingError::StaleGeneration { .. })
        ));
        assert!(matches!(
            handle.capabilities_current(),
            Err(ChannelBindingError::StaleGeneration { .. })
        ));
        assert!(matches!(
            handle.classify("stub-src", &serde_json::json!({"id": "1"})),
            Err(ChannelBindingError::StaleGeneration { .. })
        ));
        assert!(matches!(
            handle.describe(&target()),
            Err(ChannelError::GenerationRefused { .. })
        ));
        assert!(
            registry
                .installed_generation()
                .unwrap()
                .same_content(&second_generation)
        );
        let current = registry.plugins();
        let second_handle: Arc<dyn ChannelPlugin> = second.clone();
        assert!(Arc::ptr_eq(current[0].plugin(), &second_handle));
        assert!(current[0].ensure_bound().is_ok());
        let tables = registry.capability_tables().unwrap();
        assert_eq!(tables[0].0, "stub");
        assert_eq!(first.dispatches(), 0);
    }

    /// The same binding published by a different live allocation is foreign:
    /// rebuilt metadata is not the original implementation.
    #[test]
    fn a_foreign_allocation_with_identical_metadata_is_refused() {
        let mut registry = ChannelRegistry::new();
        let source = Arc::new(MutableSource::new());
        let generation = GenerationRef::new(1, Digest::blake3(b"gen"));
        let original = Arc::new(StubChannel::new("stub", "stub-src", "stub-tag", "original"));
        source.install(
            generation.clone(),
            vec![prepared(&original, b"artifact", b"descriptor")],
        );
        registry.attach_prepared_source(source.clone()).unwrap();
        let handle = registry.channel("stub").unwrap();

        let replacement = Arc::new(StubChannel::new("stub", "stub-src", "stub-tag", "replacement"));
        source.install(
            generation,
            vec![prepared(&replacement, b"artifact", b"descriptor")],
        );

        assert!(matches!(
            handle.ensure_bound(),
            Err(ChannelBindingError::ForeignPlugin { .. })
        ));
        assert!(matches!(
            handle.describe(&target()),
            Err(ChannelError::GenerationRefused { .. })
        ));
        assert_eq!(original.dispatches(), 0);
        assert_eq!(replacement.dispatches(), 0);
    }

    /// Attachment is single-shot, requires a published generation, and a
    /// second catalog cannot replace the installed port.
    #[test]
    fn prepared_attachment_is_single_shot_and_requires_a_generation() {
        let mut registry = ChannelRegistry::new();
        let unpublished = Arc::new(MutableSource::new());
        assert!(matches!(
            registry.attach_prepared_source(unpublished),
            Err(ChannelBindingError::Attachment(
                PreparedAttachmentError::NoGenerationPublished
            ))
        ));

        let source = Arc::new(MutableSource::new());
        source.install(
            GenerationRef::new(1, Digest::blake3(b"gen-1")),
            vec![prepared(
                &Arc::new(StubChannel::new("stub", "stub-src", "stub-tag", "one")),
                b"artifact-1",
                b"descriptor-1",
            )],
        );
        registry.attach_prepared_source(source.clone()).unwrap();
        // Re-attaching the exact source is idempotent.
        registry.attach_prepared_source(source).unwrap();

        let successor = Arc::new(MutableSource::new());
        successor.install(
            GenerationRef::new(2, Digest::blake3(b"gen-2")),
            vec![prepared(
                &Arc::new(StubChannel::new("stub", "stub-src", "stub-tag", "two")),
                b"artifact-2",
                b"descriptor-2",
            )],
        );
        assert!(matches!(
            registry.attach_prepared_source(successor),
            Err(ChannelBindingError::Attachment(
                PreparedAttachmentError::DifferentGeneration { .. }
            ))
        ));
    }

    /// A prepared channel must name a channel role and a well-formed identity;
    /// duplicate ids, sources or tags are refused before installation.
    #[test]
    fn prepared_catalogs_refuse_wrong_roles_and_duplicate_descriptors() {
        let plugin = Arc::new(StubChannel::new("stub", "stub-src", "stub-tag", "one"));
        assert!(matches!(
            PreparedChannel::new(
                channel_binding(PreparedRole::Provider, b"artifact", b"descriptor"),
                plugin.clone()
            ),
            Err(ChannelBindingError::WrongRole {
                role: PreparedRole::Provider,
                ..
            })
        ));
        let first = prepared(&plugin, b"artifact-1", b"descriptor-1");
        let duplicate = prepared(&plugin, b"artifact-2", b"descriptor-2");
        assert!(matches!(
            PreparedChannelCatalog::new(
                GenerationRef::new(1, Digest::blake3(b"gen")),
                vec![first, duplicate]
            ),
            Err(ChannelBindingError::Registry(
                ChannelRegistryError::DuplicateChannel
            ))
        ));
        assert!(matches!(
            PreparedChannelCatalog::new(
                GenerationRef::new(0, Digest::blake3(b"gen")),
                Vec::new()
            ),
            Err(ChannelBindingError::InvalidBinding { .. })
        ));
        let other = Arc::new(StubChannel::new("other", "other-src", "other-tag", "other"));
        assert!(
            PreparedChannelCatalog::new(
                GenerationRef::new(1, Digest::blake3(b"gen")),
                vec![
                    prepared(&plugin, b"artifact-1", b"descriptor-1"),
                    prepared(&other, b"artifact-2", b"descriptor-2"),
                ]
            )
            .is_ok()
        );
    }

    /// Compiled sealing is content-derived: the same executable and declared
    /// contracts name the same generation regardless of publication sequence
    /// and install order; a changed executable or declaration does not.
    #[test]
    fn compiled_generation_identity_is_content_derived() {
        let executable = Digest::blake3(b"executable");
        let first = Arc::new(StubChannel::new("stub-a", "source-a", "tag-a", "a"));
        let second = Arc::new(StubChannel::new("stub-b", "source-b", "tag-b", "b"));
        let original = prepare_compiled_channels(
            1,
            executable.clone(),
            vec![
                first.clone() as Arc<dyn ChannelPlugin>,
                second.clone() as Arc<dyn ChannelPlugin>,
            ],
        )
        .unwrap();
        let again = prepare_compiled_channels(
            9,
            executable.clone(),
            vec![
                Arc::new(StubChannel::new("stub-b", "source-b", "tag-b", "b"))
                    as Arc<dyn ChannelPlugin>,
                Arc::new(StubChannel::new("stub-a", "source-a", "tag-a", "a"))
                    as Arc<dyn ChannelPlugin>,
            ],
        )
        .unwrap();
        assert!(original.generation().same_content(again.generation()));
        assert_eq!(original.channels().len(), 2);
        // Installation order survives in discovery even though the identity
        // is order-independent.
        assert_eq!(original.channels()[0].channel_id(), "stub-a");
        assert_eq!(original.channels()[1].channel_id(), "stub-b");
        let same_role = again
            .channels()
            .iter()
            .find(|channel| channel.channel_id() == "stub-a")
            .unwrap();
        assert!(original.channels()[0].binding().matches_exact(same_role.binding()));

        let other_executable = prepare_compiled_channels(
            1,
            Digest::blake3(b"other-executable"),
            vec![
                first.clone() as Arc<dyn ChannelPlugin>,
                second.clone() as Arc<dyn ChannelPlugin>,
            ],
        )
        .unwrap();
        assert!(!original.generation().same_content(other_executable.generation()));

        let changed_declaration = prepare_compiled_channels(
            1,
            executable.clone(),
            vec![
                Arc::new(StubChannel::new("stub-a", "source-a", "tag-a", "changed"))
                    as Arc<dyn ChannelPlugin>,
                second as Arc<dyn ChannelPlugin>,
            ],
        )
        .unwrap();
        assert!(!original.generation().same_content(changed_declaration.generation()));
    }

    /// The refusal token is part of the durable fact vocabulary.
    #[test]
    fn generation_refusal_tokens_are_stable() {
        let refused = AttachmentError::GenerationRefused {
            channel: "stub".into(),
            attachment_id: "a1".into(),
            detail: "stale".into(),
        };
        assert_eq!(refused.token(), "generation_refused");
    }
}
