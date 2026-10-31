//! IM-channel plugin boundary — one descriptor per outbound/ingress channel.
//!
//! The subject core never names a concrete messaging platform. A channel
//! plugin registers which `[reply:<kind>:<id>]` audience kinds it accepts,
//! the localized grammar fragment shown to the model, and how an ingress
//! payload projects to a replyable target label. The reply parser, the staged
//! message header and the outbound loop all resolve through the registry, so
//! adding a channel requires no edit inside `shenko-subject` or `shenko-types`.
use serde_json::Value;

/// One audience kind a channel can deliver a reply to.
///
/// `tag` is the middle segment of `[reply:<tag>:<id>]`; `id_hint` is the
/// localized placeholder printed in the reply grammar (e.g. a group number or
/// account name); `audience` is the human noun phrase used in the
/// sender-label grammar line. Reply ids are positive integers today — that is
/// the sealed wire shape; a channel needing a wider id domain is a schema
/// change, not a registry parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplyKind {
    pub tag: &'static str,
    pub id_hint: &'static str,
    pub audience: &'static str,
}

/// Descriptor for one IM channel plugin.
#[derive(Debug, Clone)]
pub struct ChannelPlugin {
    /// Stable channel id stored in `PublicationTarget.channel`.
    pub channel: &'static str,
    /// `InboxEvent.source` values this channel produces. At most one plugin
    /// may claim a given source string.
    pub sources: &'static [&'static str],
    /// Reply audience kinds this channel accepts. A channel with no kinds is
    /// read-only for the model (its events still stage).
    pub kinds: &'static [ReplyKind],
    /// Comma-joined list of replyable label nouns for the grammar line, e.g.
    /// "account or room". Required nonempty when `kinds` is nonempty.
    pub prompt_names: &'static str,
    /// Project an ingress payload to the replyable target label shown in the
    /// staged `[who · label]` header, or `None` when the event carries no
    /// replyable account. Channel-owned because only the plugin knows which
    /// payload fields name its audiences.
    pub staged_target: fn(&Value) -> Option<String>,
}

/// Immutable set of registered channel plugins.
#[derive(Debug, Clone, Default)]
pub struct ChannelRegistry {
    plugins: Vec<ChannelPlugin>,
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
    #[error("reply tag claimed by two channels")]
    DuplicateTag,
}

fn token_ok(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self { plugins: vec![] }
    }

    /// Validate and install one plugin. Descriptors are data: uniqueness and
    /// shape are enforced here so the model grammar cannot be ambiguous.
    pub fn register(&mut self, plugin: ChannelPlugin) -> Result<(), ChannelRegistryError> {
        if !token_ok(plugin.channel) {
            return Err(ChannelRegistryError::InvalidChannel);
        }
        if !plugin.kinds.is_empty() && plugin.prompt_names.is_empty() {
            return Err(ChannelRegistryError::InvalidGrammar);
        }
        for source in plugin.sources {
            if !token_ok(source) {
                return Err(ChannelRegistryError::InvalidSource);
            }
        }
        for kind in plugin.kinds {
            if !token_ok(kind.tag) {
                return Err(ChannelRegistryError::InvalidTag);
            }
            if kind.id_hint.is_empty() || kind.audience.is_empty() {
                return Err(ChannelRegistryError::InvalidGrammar);
            }
        }
        if self.plugins.iter().any(|p| p.channel == plugin.channel) {
            return Err(ChannelRegistryError::DuplicateChannel);
        }
        for existing in &self.plugins {
            if existing
                .sources
                .iter()
                .any(|s| plugin.sources.contains(s))
            {
                return Err(ChannelRegistryError::DuplicateSource);
            }
            if existing
                .kinds
                .iter()
                .any(|k| plugin.kinds.iter().any(|n| n.tag == k.tag))
            {
                return Err(ChannelRegistryError::DuplicateTag);
            }
        }
        self.plugins.push(plugin);
        Ok(())
    }

    /// All registered plugins, in registration order.
    pub fn plugins(&self) -> impl Iterator<Item = &ChannelPlugin> {
        self.plugins.iter()
    }

    /// Resolve a `[reply:<tag>:…]` audience kind to its channel.
    /// Unregistered tags stay a parse error at the model boundary.
    pub fn reply_channel(&self, tag: &str) -> Option<&'static str> {
        self.plugins.iter().find_map(|plugin| {
            plugin
                .kinds
                .iter()
                .any(|kind| kind.tag == tag)
                .then_some(plugin.channel)
        })
    }

    /// Find the channel plugin that produces `InboxEvent.source` `source`.
    pub fn channel_for_source(&self, source: &str) -> Option<&ChannelPlugin> {
        self.plugins
            .iter()
            .find(|plugin| plugin.sources.contains(&source))
    }
}
