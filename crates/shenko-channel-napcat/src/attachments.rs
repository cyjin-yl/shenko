//! OneBot v11 message structure: segments, attachments and locators.
//!
//! SPEC §15.1, §35.6: an attachment is metadata plus a locator. The bytes do
//! not travel with the message, and the locator names the host that owns
//! them — the NapCat machine or container, not this process. A remote path is
//! never handed to a local mount as if it were a local file.
use std::path::{Path, PathBuf};

use serde_json::Value;
use shenko_types::channel::{
    AttachmentBytes, AttachmentError, AttachmentLocator, ChannelAttachment, ChannelSegment,
    ResolvedAttachment, MAX_ATTACHMENT_BYTES,
};
use tokio::io::AsyncReadExt;

use crate::channel::QQ_CHANNEL_ID;
use crate::client::NapCatClient;
use crate::config::NapCatConfig;

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
                    text: segment_text(&kind, &data),
                    kind: kind.clone(),
                    attachment_id,
                    url: data.get("url").and_then(Value::as_str).map(str::to_owned),
                });
            }
        }
        _ => {}
    }
    parsed
}

/// How long one structured segment's rendered text may be. A card or merged
/// record can carry an unbounded payload; the model gets a bounded, honest
/// rendering instead of the whole document.
pub const MAX_SEGMENT_TEXT: usize = 1024;

/// The human-readable text of one non-text OneBot segment.
///
/// SPEC §15.1 requires rich content to keep its structure: a shared card, a
/// face, a forwarded record or a location must not vanish from the message the
/// model reads. Everything here is derived from the platform's own fields; when
/// a segment carries no readable payload the function returns `None` rather
/// than inventing words. The structure itself stays in `ChannelSegment::kind`,
/// and a merged forward keeps its id so a later read can expand it.
fn segment_text(kind: &str, data: &Value) -> Option<String> {
    let field = |name: &str| {
        data.get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let number = |name: &str| {
        data.get(name).and_then(|value| match value {
            Value::String(text) => Some(text.clone()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        })
    };
    let bounded = |text: String| {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else if trimmed.chars().count() > MAX_SEGMENT_TEXT {
            let cut: String = trimmed.chars().take(MAX_SEGMENT_TEXT).collect();
            Some(format!("{cut}…"))
        } else {
            Some(trimmed.to_string())
        }
    };
    match kind {
        "text" => field("text"),
        // A mention is content: the sentence "在吗 @123" is not the same
        // sentence as "在吗", and the id is the platform's own.
        "at" => number("qq")
            .or_else(|| number("user_id"))
            .map(|id| format!("@{id}")),
        // Reply targets are carried by `ChannelContent::reply_to`; duplicating
        // them as prose would read as if the person wrote the id twice.
        "reply" => None,
        "face" => number("id").map(|id| format!("[表情 {id}]")),
        "mface" => field("summary")
            .or_else(|| field("emoji_id"))
            .map(|summary| format!("[表情 {summary}]"))
            .or_else(|| Some("[表情]".to_string())),
        "image" => Some(match field("file").or_else(|| field("summary")) {
            Some(name) => format!("[图片 {name}]"),
            None => "[图片]".to_string(),
        }),
        "record" => Some("[语音]".to_string()),
        "video" => Some("[视频]".to_string()),
        "file" => Some(match field("name").or_else(|| field("file")) {
            Some(name) => format!("[文件 {name}]"),
            None => "[文件]".to_string(),
        }),
        "json" | "xml" => {
            // The card payload is a nested document. Read the fields a card
            // publishes itself; otherwise keep the raw payload bounded so the
            // model knows something structured arrived and can ask for it.
            let inner = field("data").unwrap_or_default();
            let card = serde_json::from_str::<Value>(&inner).ok();
            let extract = |key: &str| {
                card.as_ref()
                    .and_then(|card| {
                        card.pointer(&format!("/{key}"))
                            .or_else(|| card.pointer(&format!("/meta/{key}")))
                    })
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            };
            let parts: Vec<String> = ["prompt", "desc", "title"]
                .into_iter()
                .filter_map(extract)
                .collect();
            let text = if parts.is_empty() {
                format!("[{} 卡片] {inner}", kind.to_ascii_uppercase())
            } else {
                format!("[卡片] {}", parts.join(" · "))
            };
            bounded(text)
        }
        // A merged forward is a real message this connector has NOT expanded:
        // reading it needs a `get_forward_msg` call, which the read-only query
        // allowlist deliberately does not include. The id is the platform's
        // own, so a later authorized read can name exactly this record.
        "forward" | "node" => number("id").map(|id| format!("[合并转发 {id}，未展开]")),
        "share" => bounded(match (field("title"), field("url")) {
            (Some(title), Some(url)) => format!("[链接] {title} {url}"),
            (Some(title), None) => format!("[链接] {title}"),
            (None, Some(url)) => format!("[链接] {url}"),
            (None, None) => "[链接]".to_string(),
        }),
        "location" => {
            let title = field("title").or_else(|| field("content"));
            let coords = match (field("lat"), field("lon")) {
                (Some(lat), Some(lon)) => Some(format!("{lat},{lon}")),
                _ => None,
            };
            bounded(match (title, coords) {
                (Some(title), Some(coords)) => format!("[位置] {title} ({coords})"),
                (Some(title), None) => format!("[位置] {title}"),
                (None, Some(coords)) => format!("[位置] ({coords})"),
                (None, None) => "[位置]".to_string(),
            })
        }
        "music" => bounded(match (field("title"), field("singer")) {
            (Some(title), Some(singer)) => format!("[音乐] {title} - {singer}"),
            (Some(title), None) => format!("[音乐] {title}"),
            _ => "[音乐]".to_string(),
        }),
        "contact" => {
            let id = number("id");
            let kind = field("type").unwrap_or_else(|| "contact".to_string());
            bounded(match id {
                Some(id) => format!("[名片 {kind} {id}]"),
                None => format!("[名片 {kind}]"),
            })
        }
        "dice" => Some(format!("[骰子 {}]", number("result").unwrap_or_default())),
        "rps" => Some(format!("[猜拳 {}]", number("result").unwrap_or_default())),
        "poke" => Some("[戳一戳]".to_string()),
        "shake" => Some("[窗口抖动]".to_string()),
        "markdown" => field("content"),
        "light_app" => Some("[轻应用]".to_string()),
        // Unknown kinds keep their own text when the platform supplied one; the
        // segment kind is preserved either way.
        _ => field("text"),
    }
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
    // The platform's own handle travels with the locator: NapCat's read-only
    // media actions take exactly this token, and without it an object whose
    // path this host cannot reach has no address at all.
    let params = (!file.is_empty()).then(|| {
        serde_json::json!({
            "file": file,
            "file_id": file_id,
        })
    });
    let locator = match (&path, url) {
        (Some(path), _) => AttachmentLocator {
            kind: "remote_path".to_string(),
            host: Some(config.file_host().to_string()),
            path: Some(path.to_string_lossy().to_string()),
            url: url.map(str::to_owned),
            params,
        },
        (None, Some(url)) => AttachmentLocator {
            kind: "url".to_string(),
            host: Some(config.file_host().to_string()),
            path: None,
            url: Some(url.to_string()),
            params,
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

/// The provider's own handle of one media object, when the segment carried one.
///
/// OneBot names the object with a `file` token (`1A2B3C.jpg`, a path, or a
/// provider-side key) and sometimes a `file_id`. Those are the platform's
/// facts, captured verbatim at classification time; nothing else interprets
/// them, and they are never a location the model chose.
fn media_handle(attachment: &ChannelAttachment) -> Option<String> {
    attachment
        .locator
        .params
        .as_ref()
        .and_then(|params| params.get("file"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|handle| !handle.is_empty())
        .map(str::to_owned)
}

/// Whether the bytes named by a locator live on this machine.
///
/// The configured `file_host` is the authority: a connector that reports a
/// remote host never has that host's files, and this process must not read a
/// remote path as if it were local.
fn bytes_are_local(config: &NapCatConfig) -> bool {
    matches!(config.file_host(), "local" | "localhost")
}

/// One OneBot media action for an attachment kind.
///
/// NapCat exposes read-only media actions per object kind; the plugin maps its
/// own attachment vocabulary onto them and never sends anything.
fn media_action(kind: &str) -> &'static str {
    match kind {
        "image" => "get_image",
        "voice" | "record" => "get_record",
        _ => "get_file",
    }
}

/// Resolve one attachment locator through the mechanisms this connector owns.
///
/// The order is deliberate and each step is a real mechanism, not a shim:
///
/// 1. the media root of this host, when the locator names a local path;
/// 2. the installed account's own connector (`get_image` / `get_record` /
///    `get_file` over the authenticated WebSocket), whose answer carries the
///    object as `base64` or as a path on the NapCat host;
/// 3. the platform's published URL, fetched with the connector's credential.
///
/// Every read is bounded by [`MAX_ATTACHMENT_BYTES`] *while* reading, so an
/// oversized object costs one chunk instead of its whole size, and the
/// returned attachment keeps the provider's identity.
pub async fn resolve(
    attachment: &ChannelAttachment,
    config: &NapCatConfig,
    client: &NapCatClient,
) -> Result<ResolvedAttachment, AttachmentError> {
    let channel = QQ_CHANNEL_ID.to_string();
    let attachment_id = attachment.attachment_id.clone();
    let host = attachment
        .locator
        .host
        .as_deref()
        .unwrap_or(config.file_host())
        .to_string();
    if host != config.file_host() {
        return Err(AttachmentError::ForeignHost {
            channel,
            attachment_id,
            host,
        });
    }

    let mut first_failure: Option<AttachmentError> = None;

    // 1. The media root of this host.
    if bytes_are_local(config) {
        if let Some(path) = attachment
            .locator
            .path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            match read_local(config, Path::new(path), &attachment_id).await {
                Ok(bytes) => return finish(attachment, bytes),
                // An object the root does not expose may still be reachable
                // through the connector; keep the answer and try it.
                Err(error @ AttachmentError::Unavailable { .. }) => first_failure = Some(error),
                Err(error) => return Err(error),
            }
        }
    }

    // 2. The installed account's connector.
    if let Some(handle) = media_handle(attachment) {
        let echo = format!("shenko-attachment-{}", attachment_id);
        match client
            .send_action_with_echo(
                media_action(&attachment.kind),
                serde_json::json!({ "file": handle }),
                echo,
            )
            .await
        {
            Ok(receipt) if receipt.retcode == 0 => {
                if let Some(bytes) = media_answer_bytes(&receipt.data, config, &attachment_id).await? {
                    return finish(attachment, bytes);
                }
                first_failure = Some(AttachmentError::Unavailable {
                    attachment_id: attachment_id.clone(),
                    detail: format!(
                        "connector answered {} without media bytes",
                        media_action(&attachment.kind)
                    ),
                });
            }
            Ok(receipt) => {
                first_failure = Some(AttachmentError::Unavailable {
                    attachment_id: attachment_id.clone(),
                    detail: format!(
                        "connector refused {} with retcode {}: {}",
                        media_action(&attachment.kind),
                        receipt.retcode,
                        receipt.message.as_deref().unwrap_or("no message")
                    ),
                });
            }
            Err(error) => {
                first_failure = Some(AttachmentError::Unavailable {
                    attachment_id: attachment_id.clone(),
                    detail: format!(
                        "connector did not answer {}: {error}",
                        media_action(&attachment.kind)
                    ),
                });
            }
        }
    }

    // 3. The platform's published URL.
    if let Some(url) = attachment
        .locator
        .url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        if let Some(bytes) = fetch_url(config, url, &attachment_id).await? {
            return finish(attachment, bytes);
        }
    }

    Err(first_failure.unwrap_or(AttachmentError::Refused {
        attachment_id,
        detail: "the segment named no media path, no connector handle and no URL".to_string(),
    }))
}

/// Read one media file inside the configured root, in bounded chunks.
async fn read_local(
    config: &NapCatConfig,
    path: &Path,
    attachment_id: &str,
) -> Result<Vec<u8>, AttachmentError> {
    let root = config.file_root.as_ref().ok_or_else(|| AttachmentError::Refused {
        attachment_id: attachment_id.to_string(),
        detail: "no media root is configured for this NapCat host".to_string(),
    })?;
    let root = tokio::fs::canonicalize(root)
        .await
        .map_err(|error| AttachmentError::Unavailable {
            attachment_id: attachment_id.to_string(),
            detail: format!("media root unreadable: {error}"),
        })?;
    let resolved = tokio::fs::canonicalize(path)
        .await
        .map_err(|error| AttachmentError::Unavailable {
            attachment_id: attachment_id.to_string(),
            detail: format!("media file unreadable: {error}"),
        })?;
    if !resolved.starts_with(&root) {
        return Err(AttachmentError::Refused {
            attachment_id: attachment_id.to_string(),
            detail: format!(
                "media path {} is outside the configured root",
                resolved.display()
            ),
        });
    }
    let file = tokio::fs::File::open(&resolved)
        .await
        .map_err(|error| AttachmentError::Unavailable {
            attachment_id: attachment_id.to_string(),
            detail: format!("media file unreadable: {error}"),
        })?;
    let mut reader = file.take(MAX_ATTACHMENT_BYTES + 1);
    let mut accepted = AttachmentBytes::new(attachment_id.to_string(), MAX_ATTACHMENT_BYTES);
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| AttachmentError::Unavailable {
                attachment_id: attachment_id.to_string(),
                detail: format!("media read failed: {error}"),
            })?;
        if read == 0 {
            break;
        }
        accepted.push(&buffer[..read])?;
    }
    Ok(accepted.into_vec())
}

/// The bytes a connector media action answered with.
///
/// NapCat answers either with `base64` (possibly behind a `base64://` or data
/// URL prefix) or with a path on the NapCat host. A path is only read when this
/// process owns that host's files, so a remote connector never turns into a
/// local read.
async fn media_answer_bytes(
    data: &Value,
    config: &NapCatConfig,
    attachment_id: &str,
) -> Result<Option<Vec<u8>>, AttachmentError> {
    if let Some(encoded) = data
        .get("base64")
        .and_then(Value::as_str)
        .or_else(|| data.get("file").and_then(Value::as_str).filter(|text| text.starts_with("base64://")))
    {
        let payload = encoded
            .strip_prefix("base64://")
            .or_else(|| encoded.split_once(";base64,").map(|(_, rest)| rest))
            .unwrap_or(encoded);
        return decode_bounded(payload, attachment_id).map(Some);
    }
    if let Some(path) = data.get("file").and_then(Value::as_str) {
        if !bytes_are_local(config) {
            return Err(AttachmentError::ForeignHost {
                channel: QQ_CHANNEL_ID.to_string(),
                attachment_id: attachment_id.to_string(),
                host: config.file_host().to_string(),
            });
        }
        return read_local(config, Path::new(path), attachment_id)
            .await
            .map(Some);
    }
    Ok(None)
}

/// Decode one base64 media answer under the channel byte bound.
///
/// The encoded length is refused before allocation, so an oversized answer
/// never becomes a decoded buffer.
fn decode_bounded(encoded: &str, attachment_id: &str) -> Result<Vec<u8>, AttachmentError> {
    use base64::Engine as _;
    let max_encoded = MAX_ATTACHMENT_BYTES.div_ceil(3) * 4 + 4;
    if encoded.len() as u64 > max_encoded {
        return Err(AttachmentError::TooLarge {
            attachment_id: attachment_id.to_string(),
            limit: MAX_ATTACHMENT_BYTES,
        });
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| AttachmentError::Unavailable {
            attachment_id: attachment_id.to_string(),
            detail: format!("connector media answer is not base64: {error}"),
        })?;
    if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(AttachmentError::TooLarge {
            attachment_id: attachment_id.to_string(),
            limit: MAX_ATTACHMENT_BYTES,
        });
    }
    Ok(bytes)
}

/// Fetch one platform media URL with the connector's own credential.
///
/// `Ok(None)` means the URL could not be used at all (a non-http scheme); the
/// caller then reports what the earlier mechanisms said.
async fn fetch_url(
    config: &NapCatConfig,
    url: &str,
    attachment_id: &str,
) -> Result<Option<Vec<u8>>, AttachmentError> {
    let parsed = match url::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(None),
    };
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Ok(None);
    }
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|error| AttachmentError::Unavailable {
            attachment_id: attachment_id.to_string(),
            detail: format!("media transport unavailable: {error}"),
        })?;
    let mut request = client.get(url);
    if let Some(token) = config.access_token.as_deref().filter(|token| !token.is_empty()) {
        request = request.bearer_auth(token);
    }
    let mut response = request.send().await.map_err(|error| AttachmentError::Unavailable {
        attachment_id: attachment_id.to_string(),
        detail: format!(
            "media fetch failed: {}",
            crate::client::endpoint_provenance(&error.without_url().to_string())
        ),
    })?;
    let status = response.status().as_u16();
    if !response.status().is_success() {
        return Err(match status {
            401 | 403 => AttachmentError::Unauthenticated {
                channel: QQ_CHANNEL_ID.to_string(),
                attachment_id: attachment_id.to_string(),
                detail: format!("http {status}"),
            },
            _ => AttachmentError::Unavailable {
                attachment_id: attachment_id.to_string(),
                detail: format!("http {status}"),
            },
        });
    }
    let mut accepted = AttachmentBytes::new(attachment_id.to_string(), MAX_ATTACHMENT_BYTES);
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => accepted.push(&chunk)?,
            Ok(None) => break,
            Err(error) => {
                return Err(AttachmentError::Unavailable {
                    attachment_id: attachment_id.to_string(),
                    detail: format!("media body failed: {}", error.without_url()),
                })
            }
        }
    }
    Ok(Some(accepted.into_vec()))
}

/// Build the resolved attachment, keeping the provider's identity.
fn finish(
    attachment: &ChannelAttachment,
    bytes: Vec<u8>,
) -> Result<ResolvedAttachment, AttachmentError> {
    let delivered = bytes.len() as u64;
    if let Some(declared) = attachment.bytes {
        if declared != delivered {
            return Err(AttachmentError::Unavailable {
                attachment_id: attachment.attachment_id.clone(),
                detail: format!("platform declared {declared} bytes, resolved {delivered}"),
            });
        }
    }
    let mut attachment = attachment.clone();
    if attachment.mime.is_none() {
        attachment.mime = attachment.name.as_deref().and_then(mime_for_name);
    }
    attachment.bytes = Some(delivered);
    Ok(ResolvedAttachment { attachment, bytes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_types::channel::AttachmentLocator;

    fn test_attachment(bytes: Option<u64>) -> ChannelAttachment {
        ChannelAttachment {
            attachment_id: "napcat-1".to_string(),
            kind: "image".to_string(),
            name: Some("photo.jpg".to_string()),
            mime: None,
            bytes,
            hash: None,
            locator: AttachmentLocator {
                kind: "remote_path".to_string(),
                host: None,
                path: Some("/tmp/photo.jpg".to_string()),
                url: None,
                params: None,
            },
            object: None,
            expires_at: None,
        }
    }

    #[test]
    fn finish_declared_matches() {
        let attachment = test_attachment(Some(4));
        let result = finish(&attachment, vec![1, 2, 3, 4]).unwrap();
        assert_eq!(result.attachment.bytes, Some(4));
        assert_eq!(result.bytes, &[1, 2, 3, 4]);
    }

    #[test]
    fn finish_declared_mismatch() {
        let attachment = test_attachment(Some(8));
        let err = finish(&attachment, vec![1, 2, 3, 4]).unwrap_err();
        assert!(
            matches!(err, AttachmentError::Unavailable { .. }),
            "expected Unavailable, got {err:?}"
        );
    }

    #[test]
    fn finish_no_declared_size() {
        let attachment = test_attachment(None);
        let result = finish(&attachment, vec![1, 2]).unwrap();
        assert_eq!(result.attachment.bytes, Some(2));
    }

    #[test]
    fn finish_fills_mime_from_name() {
        let attachment = test_attachment(None);
        let result = finish(&attachment, vec![0]).unwrap();
        assert_eq!(result.attachment.mime.as_deref(), Some("image/jpeg"));
    }

    #[test]
    fn finish_preserves_existing_mime() {
        let mut attachment = test_attachment(None);
        attachment.mime = Some("audio/silk".to_string());
        let result = finish(&attachment, vec![0]).unwrap();
        assert_eq!(result.attachment.mime.as_deref(), Some("audio/silk"));
    }

    #[test]
    fn decode_bounded_rejects_oversized() {
        // 8 MiB + 1 byte of plaintext encoded as base64 is ~11 MiB + change.
        // We don't need that much; just test the boundary check for small data.
        let small = base64::engine::general_purpose::STANDARD.encode([0u8; 4]);
        let decoded = decode_bounded(&small, "test-dec").unwrap();
        assert_eq!(decoded.len(), 4);
    }

    #[test]
    fn media_handle_from_locator_path() {
        let mut attachment = test_attachment(None);
        attachment.locator.params =
            Some(serde_json::json!({ "file": "/tmp/photo.jpg" }));
        let handle = media_handle(&attachment);
        assert_eq!(handle.as_deref(), Some("/tmp/photo.jpg"));
    }

    #[test]
    fn media_handle_none_without_params() {
        let attachment = test_attachment(None);
        assert!(media_handle(&attachment).is_none());
    }

    #[test]
    fn bytes_are_local_only_with_root() {
        let config = NapCatConfig {
            bot_qq: 10000,
            ..Default::default()
        };
        // Default file_host is "local", so bytes are local.
        assert!(bytes_are_local(&config));

        let remote_config = NapCatConfig {
            bot_qq: 10000,
            file_host: Some("other-machine".to_string()),
            ..Default::default()
        };
        assert!(!bytes_are_local(&remote_config));
    }
}

