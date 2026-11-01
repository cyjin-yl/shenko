//! Official QQ, WeChat iLink and Feishu/Lark channel adapters.
//!
//! Each adapter is a `shenko_types::channel::ChannelPlugin` that owns one
//! platform's protocol: the authorisation flow, the ingress classification,
//! the actual send/edit/retract/typing/attachment calls and the acceptance
//! verdict. The Shenko core only ever sees the normalized
//! [`shenko_types::channel::ChannelEnvelope`] and the opaque
//! [`shenko_types::output::PublicationTarget`]; the platform's own strings
//! never leak past this boundary.
//!
//! The differences between the platforms are honest facts, not preferences:
//!
//! * the official QQ platform has edit, retract and a channel-only typing
//!   endpoint, and serves groups, single chats and guild channels through
//!   per-application OpenIDs;
//! * the WeChat iLink protocol has no edit, no retract and no typing, serves
//!   a direct conversation only, and stores media on an AES-128-ECB CDN;
//! * Feishu/Lark has edit and retract and a typing endpoint, serves chats
//!   and reply threads, and stores media on its own drive with a typed
//!   message builder.
//!
//! SPEC §15.3 (channel adapter boundary), §16 (platform account model), §35.6
//! (channel plugin seam).

pub mod aes;
pub mod common;
pub mod feishu;
pub mod ilink;
pub mod qq_official;
pub mod runtime;

pub use common::{
    ChannelCapabilities, ChannelHealth, HealthState, PlatformAnswer, PlatformHttp, ReplyContext,
    ReplyContextCache, redact_transport_error,
};

/// One inbound event off a channel monitor.
///
/// The envelope is the classified, provider-agnostic view; `raw` is the
/// provider's own payload kept as evidence; `source` is the transport label
/// the event arrived on.
#[derive(Debug, Clone)]
pub struct InboundEvent {
    /// The normalized envelope.
    pub envelope: shenko_types::channel::ChannelEnvelope,
    /// The provider's own payload, kept as evidence.
    pub raw: serde_json::Value,
    /// The `InboxEvent.source` label of the connector that produced it.
    pub source: String,
}
