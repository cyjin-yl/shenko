//! The official QQ surface syntax: what a body means on this platform.
//!
//! SPEC §57.4: the kernel keeps ONE reply verb (`[reply:kind:id]`), while a
//! channel plugin owns its own inline surface syntax and its platform
//! rendering. On the official QQ Bot API that means three things the plain
//! string could not do:
//!
//! * `@<member_openid>` addresses a person. The platform replaces a mention
//!   with `<@` + the member openid + `>` in the message content (official
//!   docs: 群消息（全量模式）— `content`), so a literal `@7A3B...` in the text
//!   is not a mention.
//! * `[quote:<message_id>]` answers one specific message. The platform
//!   carries that as `message_reference.message_id` (official docs: 发送群聊
//!   消息 — 请求体), not as text.
//! * Markdown is a real message type here, but only in some scopes:
//!   `msg_type: 2` with a `markdown` body is open to every bot in private
//!   and group chats, while a guild channel still needs an invitation
//!   (official docs: Markdown 消息 — 2026/04/23 能力更新说明). The supported
//!   subset is headings, bold/italic/strikethrough, links, images, lists,
//!   block quotes and rules — but not code fences.
//!
//! Everything here is the plugin's own syntax and its own rendering. The
//! kernel contract, the durable ledger, the account scope and the acceptance
//! predicate do not change: only what the wire carries does. A body with no
//! markers is sent exactly as before — one plain text message — so the
//! ordinary path keeps its current bytes.

use serde_json::{Value, json};

/// The scope a body is sent into.
///
/// The scope decides whether the platform can show Markdown at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundScope {
    /// A private (C2C) chat. Markdown is available.
    Private,
    /// A group chat. Markdown is available.
    Group,
    /// A guild channel. Markdown needs an invitation this installation does
    /// not have, so the body degrades to plain text.
    Channel,
}

impl OutboundScope {
    /// Whether the platform can render Markdown in this scope.
    pub fn supports_markdown(self) -> bool {
        matches!(self, OutboundScope::Private | OutboundScope::Group)
    }

    /// The platform's own mention markup for this scope.
    ///
    /// The two platforms spell a mention differently: a group message
    /// replaces a mention with `<@` + the member openid + `>` (official docs:
    /// 群消息（全量模式）— `content`), while a guild channel message uses
    /// `<@!id>` (official docs: 发送子频道消息 — JSON 格式示例). A private
    /// chat has nobody to mention, so a mention there stays literal text.
    pub fn mention_markup(self, openid: &str) -> Option<String> {
        match self {
            OutboundScope::Group => Some(format!("<@{openid}>")),
            OutboundScope::Channel => Some(format!("<@!{openid}>")),
            OutboundScope::Private => None,
        }
    }
}

/// The official message one body maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfficialOutbound {
    /// The platform message type: 0 for text, 2 for Markdown.
    pub msg_type: u32,
    /// The text body. Empty when the message is Markdown.
    pub content: String,
    /// The Markdown body. Present only when the message is Markdown.
    pub markdown: Option<String>,
    /// The quoted message id, when the body quotes one message.
    pub quote: Option<String>,
    /// The mentioned member openids, in the order they appeared.
    pub mentions: Vec<String>,
}

impl OfficialOutbound {
    /// The wire body for the official send endpoints.
    ///
    /// The platform refuses a message that carries both `content` and
    /// `markdown`, so exactly one of them is present.
    pub fn wire_body(&self) -> Value {
        let mut body = if let Some(markdown) = &self.markdown {
            json!({"msg_type": 2, "markdown": {"content": markdown}})
        } else {
            json!({"msg_type": self.msg_type, "content": self.content})
        };
        if let Some(quote) = &self.quote {
            body["message_reference"] = json!({"message_id": quote});
        }
        body
    }
}

/// Build the official message for one body.
///
/// The markers are the plugin's own syntax: they are removed from the text
/// and become platform structure. An unknown or malformed marker stays
/// literal text — the body is never lost to a parse error.
pub fn build_outbound(body: &str, scope: OutboundScope) -> OfficialOutbound {
    let mut text = body.trim().to_string();
    let mut mentions: Vec<String> = Vec::new();

    // A leading `@<member_openid>` addresses one person. The platform's own
    // markup is `<@openid>`; only the first mention becomes one, because a
    // later `@` is usually part of a word.
    if let Some(rest) = text.strip_prefix('@') {
        let token: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if looks_like_openid(&token) {
            let remainder = rest[token.len()..].trim_start();
            match scope.mention_markup(&token) {
                Some(markup) => {
                    mentions.push(token.clone());
                    text = format!("{markup} {remainder}").trim().to_string();
                }
                // A private chat has nobody to mention: the text stays as it
                // was written rather than carrying markup nothing would read.
                None => {}
            }
        }
    }

    // `[quote:<message_id>]` answers one message. Only the first quote is
    // carried; further markers stay literal so nothing is silently dropped.
    let mut quote = None;
    if let Some(start) = text.find("[quote:") {
        let after = &text[start + "[quote:".len()..];
        if let Some(end) = after.find(']') {
            let message_id = after[..end].trim();
            if !message_id.is_empty() {
                let remainder = format!("{}{}", &text[..start], &after[end + 1..]);
                quote = Some(message_id.to_string());
                text = remainder.trim().to_string();
            }
        }
    }

    // A body that mentions somebody is sent as text: the platform documents
    // the mention markup for message CONTENT, and the Markdown document does
    // not mention it, so a mention that only exists in a Markdown body would
    // silently be a name, not a mention.
    let markdown_body = if scope.supports_markdown() && mentions.is_empty() && carries_markdown(&text) {
        Some(to_platform_markdown(&text))
    } else {
        None
    };

    match markdown_body {
        Some(markdown) => OfficialOutbound {
            msg_type: 2,
            content: String::new(),
            markdown: Some(markdown),
            quote,
            mentions,
        },
        None => OfficialOutbound {
            msg_type: 0,
            content: downgrade_markdown(&text),
            markdown: None,
            quote,
            mentions,
        },
    }
}

/// Whether a body carries Markdown the platform would render.
///
/// The platform renders headings, emphasis, strikethrough, links, images,
/// lists, block quotes and rules. A body with none of them is plain text,
/// and saying so keeps the ordinary path on one plain message.
fn carries_markdown(text: &str) -> bool {
    let mut fenced = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            // A fence is not Markdown for this platform, but its content is
            // not Markdown either: the body is text with a code block.
            return false;
        }
        if fenced {
            continue;
        }
        if trimmed.starts_with('#')
            || trimmed.starts_with('>')
            || trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("1. ")
            || trimmed.contains("**")
            || trimmed.contains("~~")
            || trimmed.contains("](")
        {
            return true;
        }
    }
    false
}

/// Whether a token looks like a platform openid.
///
/// The official openids are long alphanumeric identifiers (`A1B2C3…`, 32
/// characters in the docs' examples). A short word after `@` is prose, not an
/// identifier, so it stays literal text.
fn looks_like_openid(token: &str) -> bool {
    token.len() >= 16 && token.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Downgrade Markdown to the plain text the platform can show as text.
///
/// The words stay; only the markers go. Fenced code keeps its content (the
/// fence itself is noise on a chat client), emphasis loses its asterisks,
/// headings lose their hashes, and a link keeps both its label and its
/// target — a reader on a chat client cannot click it, so hiding the URL
/// would lose information.
pub fn downgrade_markdown(text: &str) -> String {
    let mut out = String::new();
    let mut fenced = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            // Inside a fence the content IS the message: keep the code, lose
            // the fence.
            out.push_str(line);
            out.push('\n');
            continue;
        }
        out.push_str(&strip_inline_markdown(line));
        out.push('\n');
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Remove the inline markers of one line, keeping the words.
fn strip_inline_markdown(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // Bold, italic, strikethrough: the emphasis markers themselves
            // are noise, the words are not.
            '*' | '_' | '~' => {
                // A run of markers is dropped; the text inside it stays.
                while chars.peek().is_some_and(|next| *next == c) {
                    chars.next();
                }
            }
            // A heading's hashes are structure, not words.
            '#' if out.is_empty() => {
                while chars.peek() == Some(&'#') {
                    chars.next();
                }
                while chars.peek() == Some(&' ') {
                    chars.next();
                }
            }
            '>' if out.is_empty() => {
                while chars.peek() == Some(&'>') {
                    chars.next();
                }
                while chars.peek() == Some(&' ') {
                    chars.next();
                }
            }
            // Inline code keeps its content, loses its backticks.
            '`' => {
                while chars.peek() == Some(&'`') {
                    chars.next();
                }
            }
            _ => out.push(c),
        }
    }
    out.trim_end().to_string()
}

/// Rewrite a body into the Markdown subset the platform renders.
///
/// The platform supports headings, emphasis, links, images, lists, block
/// quotes and rules. It does NOT support code fences, so a fenced block
/// becomes plain lines. Nothing else is touched: the body that reaches the
/// platform is the body the subject wrote, minus the parts the platform
/// would show as noise.
fn to_platform_markdown(text: &str) -> String {
    let mut out = String::new();
    let mut fenced = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            // The fence itself is not Markdown for this platform; the code
            // inside it is plain text, and it stays.
            continue;
        }
        if fenced {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_body_stays_one_text_message() {
        let message = build_outbound("今晚八点，老地方见。", OutboundScope::Group);
        assert_eq!(message.msg_type, 0);
        assert_eq!(message.content, "今晚八点，老地方见。");
        assert_eq!(message.markdown, None);
        assert_eq!(message.quote, None);
        assert!(message.mentions.is_empty());
    }

    #[test]
    fn a_leading_mention_becomes_the_platform_markup() {
        let message = build_outbound(
            "@A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4 你说的那件事我查过了",
            OutboundScope::Group,
        );
        assert_eq!(message.mentions, vec!["A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4"]);
        assert!(
            message.content.starts_with("<@A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4>"),
            "{}",
            message.content
        );
        assert_eq!(message.msg_type, 0);
    }

    #[test]
    fn a_guild_channel_uses_the_platform_own_mention_markup() {
        // The guild platform spells a mention `<@!id>`; the group platform
        // spells it `<@id>`. They are not the same markup.
        let message = build_outbound(
            "@A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4 看这里",
            OutboundScope::Channel,
        );
        assert_eq!(message.mentions.len(), 1);
        assert!(
            message.content.starts_with("<@!A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4>"),
            "{}",
            message.content
        );
    }

    #[test]
    fn a_private_chat_has_nobody_to_mention() {
        let message = build_outbound(
            "@A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4 你说的那件事",
            OutboundScope::Private,
        );
        assert!(message.mentions.is_empty());
        assert!(
            message.content.starts_with("@A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4"),
            "{}",
            message.content
        );
    }

    #[test]
    fn a_word_after_the_at_sign_stays_text() {
        let message = build_outbound("@someone 帮忙看一下", OutboundScope::Group);
        assert!(message.mentions.is_empty());
        assert_eq!(message.content, "@someone 帮忙看一下");
    }

    #[test]
    fn a_quote_becomes_a_message_reference() {
        let message = build_outbound("[quote:REFIDX_abc==] 是这一条", OutboundScope::Group);
        assert_eq!(message.quote.as_deref(), Some("REFIDX_abc=="));
        assert!(!message.content.contains("[quote:"), "{}", message.content);
        assert!(message.content.contains("是这一条"), "{}", message.content);
    }

    #[test]
    fn a_mention_and_a_quote_keep_their_order() {
        let message = build_outbound(
            "@A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4 [quote:REFIDX_abc==] 这一条我查过了",
            OutboundScope::Group,
        );
        assert_eq!(message.mentions.len(), 1);
        assert_eq!(message.quote.as_deref(), Some("REFIDX_abc=="));
    }

    #[test]
    fn markdown_becomes_a_markdown_message_where_the_platform_shows_it() {
        let message = build_outbound("**结论**：今天完成。", OutboundScope::Group);
        assert_eq!(message.msg_type, 2);
        assert_eq!(message.markdown.as_deref(), Some("**结论**：今天完成。"));
        assert!(message.content.is_empty());
    }

    #[test]
    fn a_guild_channel_degrades_to_text() {
        // The official Markdown capability is open to every bot in private and
        // group chats; a guild channel still needs an invitation.
        let message = build_outbound("**结论**：今天完成。", OutboundScope::Channel);
        assert_eq!(message.msg_type, 0);
        assert_eq!(message.content, "结论：今天完成。");
        assert_eq!(message.markdown, None);
    }

    #[test]
    fn a_mention_keeps_the_body_plain_so_the_mention_is_a_mention() {
        let message = build_outbound(
            "@A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4 **看这里**",
            OutboundScope::Group,
        );
        assert_eq!(message.msg_type, 0);
        assert!(message.content.contains("<@A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4>"));
        assert!(!message.content.contains("**"), "{}", message.content);
    }

    #[test]
    fn code_fences_lose_the_fence_and_keep_the_content() {
        let text = "先看这段：\n```rust\nfn main() {}\n```\n就这样。";
        let message = build_outbound(text, OutboundScope::Channel);
        assert!(message.content.contains("fn main() {}"), "{}", message.content);
        assert!(!message.content.contains("```"), "{}", message.content);
    }

    #[test]
    fn the_wire_body_carries_exactly_one_content_field() {
        let markdown = build_outbound("**好**", OutboundScope::Group);
        let body = markdown.wire_body();
        assert_eq!(body["msg_type"], 2);
        assert!(body.get("content").is_none());
        assert_eq!(body["markdown"]["content"], "**好**");

        let text = build_outbound("**好**", OutboundScope::Channel);
        let body = text.wire_body();
        assert_eq!(body["msg_type"], 0);
        assert_eq!(body["content"], "好");
        assert!(body.get("markdown").is_none());
    }

    #[test]
    fn a_quote_reaches_the_wire_as_a_message_reference() {
        let message = build_outbound("[quote:REFIDX_abc==] 好的", OutboundScope::Group);
        let body = message.wire_body();
        assert_eq!(body["message_reference"]["message_id"], "REFIDX_abc==");
    }
}
