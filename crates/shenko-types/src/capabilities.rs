//! What a channel platform can really do — SPEC §15.3, §57.4.
//!
//! One installed plugin declares this table once, at the boundary it owns.
//! The runtime advertises exactly this set, so a platform is never described
//! by a product feature list or by what the model would like to be able to
//! do.
//!
//! A field that is `false` is a promise NOT to attempt the action: the
//! plugin refuses it before the platform ever sees a request, because a
//! promise the platform will not keep is worse than an honest refusal.

use serde::{Deserialize, Serialize};

/// What one platform can really do.
///
/// The model grammar and the runtime advertise exactly this set. A field that
/// is `false` is a promise NOT to attempt the action, not a promise that the
/// platform might accept it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelCapabilities {
    /// One-to-one conversations with a person.
    pub direct: bool,
    /// Group conversations.
    pub group: bool,
    /// Thread/topic conversations.
    pub thread: bool,
    /// Guild/channel conversations (QQ official channels).
    pub guild_channel: bool,
    /// Structured rich text beyond plain text: the platform renders styled
    /// content rather than plain characters.
    pub rich_text: bool,
    /// Media on this channel: the platform carries images, files, voice and
    /// video, and the plugin can read what it receives.
    pub attachments: bool,
    /// Editing a delivered message.
    pub edit: bool,
    /// Retracting a delivered message.
    pub retract: bool,
    /// Publishing a typing indicator.
    pub typing: bool,
    /// Sending without an inbound message (proactive delivery).
    pub proactive: bool,
    /// Human-readable notes about the real limits of the above.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}
