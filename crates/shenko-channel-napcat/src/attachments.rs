//! OneBot v11 message structure: segments, attachments and locators.
//!
//! SPEC §15.1, §35.6: an attachment is metadata plus a locator. The bytes do
//! not travel with the message, and the locator names the host that owns
//! them — the NapCat machine or container, not this process. A remote path is
//! never handed to a local mount as if it were a local file.
use std::path::{Path, PathBuf};

use serde_json::Value;
use shenko_types::channel::{AttachmentLocator, ChannelAttachment, ChannelSegment};

use crate::config::NapCatConfig;

/// The largest attachment this connector reads in one resolution.
pub const MAX_ATTACHMENT_BYTES: u64 = 8 * 1024 * 1024;

/// The parsed structure of one OneBot message.
#[derive(Debug, Clone, Default)]
pub struct ParsedMessage {
    /// The structured segments of the message.
    pub segments: Vec<ChannelSegment>,
    /// The attachments the platform named.
    pub attachments: Vec<ChannelAttachment>,
    /// The mentioned platform ids.
    pub mentions: Vec<String>,
    /// The platform id of the message this one replies to.
    pub reply_to: Option<String>,
}

/// Parse the `message` field of a OneBot event into segments and attachments.
///
/// OneBot sends either a plain string (possibly with CQ codes) or an array of
/// segment objects. Both forms are read; nothing is guessed beyond the
/// platform's own fields.
pub fn parse_message(message: &Value, config: &NapCatConfig) -> ParsedMessage {
    let mut parsed = ParsedMessage::default();
    match message {
        Value::String(text) => {
            for part in split_cq_codes(text) {
                let (kind, data) = part;
                if kind == "image" || kind == "record" || kind == "video" || kind == "file" {
                    let value = serde_json::json!({ "data": cq_data(&data) });
                    if let Some(attachment) =
                        attachment_for(&kind, &value, parsed.attachments.len(), config)
                    {
                        parsed.attachments.push(attachment.clone());
                        parsed.segments.push(ChannelSegment {
                            kind,
                            text: None,
                            attachment_id: Some(attachment.attachment_id),
                            url: None,
                        });
                        continue;
                    }
                }
                parsed.segments.push(ChannelSegment {
                    kind,
                    text: Some(data),
                    attachment_id: None,
                    url: None,
                });
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                let kind = item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let data = item.get("data").cloned().unwrap_or(Value::Null);
                if kind == "text" {
                    let text = data
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    parsed.segments.push(ChannelSegment {
                        kind: "text".to_string(),
                        text: Some(text),
                        attachment_id: None,
                        url: None,
                    });
                    continue;
                }
                if kind == "at" {
                    if let Some(mention) = data
                        .get("qq")
 .or_else(|| data.get("user_id"))
                        .and_then(Value::as_str)
                        .filter(|mention| !mention.is_empty())
                    {
                        parsed.mentions.push(mention.to_string());
                    }
                }
                if kind == "reply" {
                    if let Some(replied) = data.get("id").and_then(Value::as_str) {
                        parsed.reply_to = Some(replied.to_string());
                    }
                }
                let attachment = attachment_for(&kind, &data, index, config);
                let attachment_id = attachment.as_ref().map(|a| a.attachment_id.clone());
                if let Some(attachment) = attachment {
                    parsed.attachments.push(attachment);
                }
                parsed.segments.push(ChannelSegment {
                    kind: kind.clone(),
                    text: data.get("text").and_then(Value::as_str).map(str::to_owned),
                    attachment_id,
                    url: data.get("url").and_then(Value::as_str).map(str::to_owned),
                });
            }
        }
        _ => {}
    }
    parsed
}

/// The `key=value` payload of a CQ code.
fn cq_data(data: &str) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    for piece in data.split(',') {
        let Some((key, value)) = piece.split_once('=') else {
            continue;
        };
        map.insert(key.trim().to_string(), Value::String(value.to_string()));
    }
    map
}

/// Split a CQ-coded message into `(kind, text)` parts.
///
/// The parser understands the CQ code shape OneBot uses (`[CQ:image,file=a]`)
/// and keeps everything it does not understand as plain text.
fn split_cq_codes(text: &str) -> Vec<(String, String)> {
    let mut parts = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[CQ:") {
        if start > 0 {
            parts.push(("text".to_string(), rest[..start].to_string()));
        }
        let tail = &rest[start..];
        let Some(end) = tail.find(']') else {
            parts.push(("text".to_string(), tail.to_string()));
            return parts;
        };
        let code = &tail[3..end];
        let mut kind = "unknown".to_string();
        let mut data = String::new();
        for (index, piece) in code.split(',').enumerate() {
            if index == 0 {
                kind = piece.trim().to_ascii_lowercase();
                continue;
            }
            if !data.is_empty() {
                data.push(',');
            }
            data.push_str(piece);
        }
        parts.push((kind, data));
        rest = &tail[end + 1..];
    }
    if !rest.is_empty() {
        parts.push(("text".to_string(), rest.to_string()));
    }
    parts
}

/// Build the attachment of one media segment, when the platform named one.
fn attachment_for(
    kind: &str,
    data: &Value,
    index: usize,
    config: &NapCatConfig,
) -> Option<ChannelAttachment> {
    if !matches!(kind, "image" | "record" | "video" | "file" | "voice") {
        return None;
    }
    let file = data.get("file").and_then(Value::as_str).unwrap_or_default();
    let url = data.get("url").and_then(Value::as_str);
    let file_id = data.get("file_id").and_then(Value::as_str);
    if file.is_empty() && url.is_none() {
        return None;
    }
    let path = resolve_media_path(file, config);
    let locator = match (&path, url) {
        (Some(path), _) => AttachmentLocator {
            kind: "remote_path".to_string(),
            host: Some(config.file_host().to_string()),
            path: Some(path.to_string_lossy().to_string()),
            url: url.map(str::to_owned),
        },
        (None, Some(url)) => AttachmentLocator {
            kind: "url".to_string(),
            host: Some(config.file_host().to_string()),
            path: None,
            url: Some(url.to_string()),
        },
        (None, None) => return None,
    };
    let name = file
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    let mime = data
        .get("mime")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| name.as_deref().and_then(mime_for_name));
    Some(ChannelAttachment {
        attachment_id: file_id
            .map(str::to_owned)
            .unwrap_or_else(|| format!("attachment-{index}")),
        kind: kind.to_string(),
        name,
        mime,
        bytes: data.get("size").and_then(Value::as_u64),
        hash: None,
        locator,
        object: None,
        expires_at: None,
    })
}

/// The path of a media file on the NapCat host, when it can be named.
///
/// An absolute path is used as reported. A bare file name is resolved inside
/// the configured media root; without a root the file cannot be located and
/// the attachment keeps only its URL.
fn resolve_media_path(file: &str, config: &NapCatConfig) -> Option<PathBuf> {
    if file.is_empty() {
        return None;
    }
    let path = Path::new(file);
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    config.file_root.as_ref().map(|root| root.join(path))
}

/// The MIME type of a media file, from its extension only.
pub fn mime_for_name(name: &str) -> Option<String> {
    let extension = name.rsplit('.').next()?.to_ascii_lowercase();
    let mime = match extension.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "amr" => "audio/amr",
        "silk" => "audio/silk",
        "m4a" => "audio/mp4",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "zip" => "application/zip",
        _ => return None,
    };
    Some(mime.to_string())
}

/// Why an attachment could not be resolved on this host.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttachmentError {
    #[error("attachment lives on host {host}; this connector resolves host {local}")]
    ForeignHost { host: String, local: String },
    #[error("no media root is configured for the NapCat host")]
    NoMediaRoot,
    #[error("attachment path is outside the configured media root")]
    OutsideMediaRoot,
    #[error("attachment file is missing")]
    Missing,
    #[error("attachment is larger than {limit} bytes")]
    TooLarge { limit: u64 },
    #[error("attachment read failed: {detail}")]
    Read { detail: String },
}

/// The bytes of one attachment, read on the host that owns them.
#[derive(Debug, Clone)]
pub struct ResolvedAttachment {
    pub attachment_id: String,
    pub bytes: Vec<u8>,
    pub mime: Option<String>,
    /// The sealed content-addressed object of the bytes, when the caller
    /// sealed them into the store.
    pub object: Option<shenko_types::object::ObjectRef>,
}

/// Read one attachment on the NapCat host.
///
/// The read is confined to the configured media root: a locator outside it is
/// refused instead of reaching into the rest of the host. A locator that
/// names another host is refused as well — that host needs its own file
/// capability, and this connector does not pretend to have it.
pub fn resolve_attachment(
    attachment: &ChannelAttachment,
    config: &NapCatConfig,
) -> Result<ResolvedAttachment, AttachmentError> {
    let host = attachment.locator.host.as_deref().unwrap_or("local");
    if host != config.file_host() {
        return Err(AttachmentError::ForeignHost {
            host: host.to_string(),
            local: config.file_host().to_string(),
        });
    }
    let root = config
        .file_root
        .as_ref()
        .ok_or(AttachmentError::NoMediaRoot)?;
    let path = attachment
        .locator
        .path
        .as_ref()
        .map(PathBuf::from)
        .ok_or(AttachmentError::Missing)?;
    let canonical_root = root
        .canonicalize()
        .map_err(|error| AttachmentError::Read {
            detail: error.to_string(),
        })?;
    let canonical_path = path
        .canonicalize()
        .map_err(|_| AttachmentError::Missing)?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(AttachmentError::OutsideMediaRoot);
    }
    let metadata = std::fs::metadata(&canonical_path).map_err(|_| AttachmentError::Missing)?;
    if metadata.len() > MAX_ATTACHMENT_BYTES {
        return Err(AttachmentError::TooLarge {
            limit: MAX_ATTACHMENT_BYTES,
        });
    }
    let bytes = std::fs::read(&canonical_path).map_err(|error| AttachmentError::Read {
        detail: error.to_string(),
    })?;
    Ok(ResolvedAttachment {
        attachment_id: attachment.attachment_id.clone(),
        bytes,
        mime: attachment.mime.clone(),
        object: None,
    })
}
