//! The QQ surface syntax: what a body means on this platform.
//!
//! docs/specs/channels.md#spec-57-4: the kernel keeps ONE reply verb (`[reply:kind:id]`), while a
//! channel plugin owns its own inline surface syntax and its platform
//! rendering. On QQ that means three things the plain string could not do:
//!
//! * `@<qq>` at the start of a body addresses a person. The platform needs a
//!   real `at` segment for that; a literal `@12345` in the text is not an
//!   mention.
//! * `[quote:<message_id>]` answers a specific message. The platform needs a
//!   `reply` segment; without it the answer floats in the group with no
//!   visible referent.
//! * Markdown markers (`**bold**`, `# heading`, fences, link syntax) are not
//!   QQ syntax. QQ shows them literally, so a body written for a rich surface
//!   arrives as noise. The body is downgraded to plain text that still says
//!   the same thing.
//!
//! Nothing here changes the kernel contract, the durable ledger, the account
//! scope or the acceptance predicate: it only decides what the wire carries.
//! A body with no markers is sent exactly as before — one text segment — so
//! the ordinary path keeps its current bytes.

use serde_json::{Value, json};

/// The OneBot segments one outbound body maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundMessage {
    /// The segments, in platform order.
    pub segments: Vec<Value>,
    /// The body with the surface markers removed and the Markdown downgraded.
    pub plain_text: String,
}

impl OutboundMessage {
    /// Whether the body carries platform structure (a mention or a quote).
    ///
    /// A body without structure is sent as one plain text segment, exactly
    /// like before this module existed.
    pub fn has_structure(&self) -> bool {
        self.segments
            .iter()
            .any(|segment| !matches!(segment.get("type").and_then(Value::as_str), Some("text")))
    }

    /// The wire value for the OneBot `message` field: a segment array when
    /// the body has structure, otherwise the plain string.
    pub fn wire_message(&self) -> Value {
        if self.has_structure() {
            Value::Array(self.segments.clone())
        } else {
            Value::String(self.plain_text.clone())
        }
    }
}

/// Build the platform message for one body.
///
/// The markers are the plugin's own syntax, not the kernel's: they are
/// stripped from the text and become segments. An unknown or malformed marker
/// stays literal text — the body is never lost to a parse error.
pub fn build_outbound(body: &str) -> OutboundMessage {
    let mut segments: Vec<Value> = Vec::new();
    let mut text = body.trim().to_string();

    // A leading `@<qq>` addresses one person. Only the first one becomes a
    // mention; a later `@` stays in the text, because in prose it is usually
    // part of a word or an address.
    if let Some(rest) = text.strip_prefix('@') {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            let remainder = rest[digits.len()..].trim_start();
            segments.push(json!({"type": "at", "data": {"qq": digits}}));
            text = remainder.to_string();
        }
    }

    // `[quote:<message_id>]` answers one message. Only the first quote is a
    // segment; further markers stay literal so nothing is silently dropped.
    if let Some(start) = text.find("[quote:") {
        let after = &text[start + "[quote:".len()..];
        if let Some(end) = after.find(']') {
            let message_id = after[..end].trim();
            if !message_id.is_empty() {
                let remainder = format!("{}{}", &text[..start], &after[end + 1..]);
                segments.push(json!({"type": "reply", "data": {"id": message_id}}));
                text = remainder.trim().to_string();
            }
        }
    }

    let plain_text = downgrade_markdown(&text);
    if !plain_text.is_empty() {
        segments.push(json!({"type": "text", "data": {"text": plain_text}}));
    }
    OutboundMessage { segments, plain_text }
}

/// Downgrade Markdown to the plain text QQ can show.
///
/// The words stay; only the markers go. Fenced code keeps its content (the
/// fence itself is noise on a chat client), emphasis loses its asterisks,
/// headings lose their hashes, and a link keeps both its label and its
/// target — a reader on QQ cannot click it, so hiding the URL would lose
/// information.
pub fn downgrade_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let mut line = line.to_string();
        // A heading marker is presentation, not content.
        let hashes = line.chars().take_while(|c| *c == '#').count();
        if hashes > 0 && hashes <= 6 && line[hashes..].starts_with(' ') {
            line = line[hashes..].trim_start().to_string();
        }
        // A list marker becomes a plain bullet, because `*` reads as emphasis.
        for marker in ["- ", "* ", "+ "] {
            if let Some(rest) = line.strip_prefix(marker) {
                line = format!("· {rest}");
                break;
            }
        }
        // A blockquote marker is presentation too.
        if let Some(rest) = line.strip_prefix("> ") {
            line = rest.to_string();
        }
        line = strip_inline_markdown(&line);
        out.push_str(&line);
        out.push('\n');
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Remove inline emphasis and link markers while keeping the words.
fn strip_inline_markdown(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let chars: Vec<char> = line.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let rest: String = chars[index..].iter().collect();
        // A link `[label](target)` keeps both parts: QQ cannot click it.
        if rest.starts_with('[') {
            if let Some(close) = rest.find(']') {
                if rest[close + 1..].starts_with('(') {
                    if let Some(paren) = rest[close + 1..].find(')') {
                        let label = &rest[1..close];
                        let target = &rest[close + 2..close + 1 + paren];
                        out.push_str(label);
                        out.push_str(" (");
                        out.push_str(target);
                        out.push(')');
                        index += close + 1 + paren + 1;
                        continue;
                    }
                }
            }
        }
        // Emphasis markers are dropped; the emphasised words stay.
        if rest.starts_with("**") || rest.starts_with("__") {
            let marker = &rest[..2];
            if let Some(end) = rest[2..].find(marker) {
                out.push_str(&rest[2..2 + end]);
                index += 2 + end + 2;
                continue;
            }
        }
        if rest.starts_with('*') || rest.starts_with('_') {
            if let Some(end) = rest[1..].find(['*', '_']) {
                out.push_str(&rest[1..1 + end]);
                index += 1 + end + 1;
                continue;
            }
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_body_stays_one_text_segment() {
        let message = build_outbound("今晚八点，老地方见。");
        assert!(!message.has_structure());
        assert_eq!(message.wire_message(), Value::String("今晚八点，老地方见。".into()));
    }

    #[test]
    fn a_leading_mention_becomes_an_at_segment() {
        let message = build_outbound("@12345 你说的那件事，我查过了");
        assert!(message.has_structure());
        let wire = message.wire_message();
        let segments = wire.as_array().expect("segments");
        assert_eq!(segments[0]["type"], "at");
        assert_eq!(segments[0]["data"]["qq"], "12345");
        assert_eq!(segments[1]["type"], "text");
        assert_eq!(segments[1]["data"]["text"], "你说的那件事，我查过了");
    }

    #[test]
    fn a_mention_that_is_not_a_number_stays_text() {
        let message = build_outbound("@someone 帮忙看一下");
        assert!(!message.has_structure());
        assert_eq!(message.plain_text, "@someone 帮忙看一下");
    }

    #[test]
    fn a_quote_becomes_a_reply_segment() {
        let message = build_outbound("[quote:998877] 是这一条，我查过了");
        let wire = message.wire_message();
        let segments = wire.as_array().expect("segments");
        assert_eq!(segments[0]["type"], "reply");
        assert_eq!(segments[0]["data"]["id"], "998877");
        assert_eq!(segments[1]["data"]["text"], "是这一条，我查过了");
    }

    #[test]
    fn a_mention_and_a_quote_keep_their_order() {
        let message = build_outbound("@12345 [quote:998877] 这一条我查过了");
        let segments = message.wire_message();
        let segments = segments.as_array().expect("segments");
        assert_eq!(segments[0]["type"], "at");
        assert_eq!(segments[1]["type"], "reply");
        assert_eq!(segments[2]["type"], "text");
    }

    #[test]
    fn markdown_markers_do_not_reach_the_user() {
        let plain = downgrade_markdown("**重要**：请看\n# 标题\n- 第一项\n- 第二项");
        assert!(!plain.contains("**"));
        assert!(!plain.contains("# 标题"));
        assert!(plain.contains("重要"));
        assert!(plain.contains("· 第一项"));
    }

    #[test]
    fn a_link_keeps_its_label_and_its_target() {
        let plain = downgrade_markdown("看这个 [文档](https://example.com/x)");
        assert_eq!(plain, "看这个 文档 (https://example.com/x)");
    }

    #[test]
    fn code_fences_lose_the_fence_and_keep_the_content() {
        let plain = downgrade_markdown("```rust\nlet x = 1;\n```");
        assert_eq!(plain, "let x = 1;");
    }
}
