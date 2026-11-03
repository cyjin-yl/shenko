//! The Feishu/Lark message builder — SPEC §16.
//!
//! A Feishu message is not a plain string: it is a `post` document of typed
//! blocks. A text paragraph is one block, an image is an `img` block naming
//! an `image_key`, a video is a `media` block naming a `file_key`, and a file
//! is a `file` block. This module turns the normalized text and the uploaded
//! media references into the document the platform accepts.

use serde_json::{Value, json};

/// One uploaded attachment ready to be placed in a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaBlock {
    /// The platform file key.
    pub file_key: String,
    /// The attachment kind: `image`, `video`, `voice` or `file`.
    pub kind: String,
    /// The display name, for a file block.
    pub name: Option<String>,
}

/// Build a `post` document from the body text and the uploaded media.
///
/// The text is the first paragraph; each attachment is its own paragraph, so
/// a reader sees the message and then the media in order.
pub fn post_content(body: &str, media: &[MediaBlock]) -> Value {
    let mut rows: Vec<Value> = Vec::new();
    if !body.is_empty() {
        rows.push(json!([{ "tag": "text", "text": body }]));
    }
    for block in media {
        rows.push(match block.kind.as_str() {
            "image" => json!([{ "tag": "img", "image_key": block.file_key }]),
            "video" => json!([{
                "tag": "media",
                "file_key": block.file_key,
                "file_name": block.name.clone().unwrap_or_else(|| "video".to_string()),
            }]),
            "voice" => json!([{
                "tag": "media",
                "file_key": block.file_key,
                "file_name": block.name.clone().unwrap_or_else(|| "voice".to_string()),
            }]),
            _ => json!([{
                "tag": "file",
                "file_key": block.file_key,
                "file_name": block.name.clone().unwrap_or_else(|| "file".to_string()),
            }]),
        });
    }
    if rows.is_empty() {
        rows.push(json!([{ "tag": "text", "text": "" }]));
    }
    json!({ "zh_cn": { "title": "", "content": rows } })
}
