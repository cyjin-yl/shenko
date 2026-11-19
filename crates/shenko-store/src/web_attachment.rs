//! Files, images and long text the owner attaches to a browser submission.
//!
//! An upload is sealed into the content-addressed store before any submission
//! names it, so a submission can only reference bytes that already exist. The
//! reference carries the owner's file name and declared media type; the bytes
//! are never rewritten. Admission checks every reference against the CAS, and
//! the subject's history renders the same reference, so what the owner sent,
//! what was admitted and what the subject saw are one fact.

use serde::{Deserialize, Serialize};

use shenko_types::error::ShenkoError;
use shenko_types::object::Digest;

use crate::store::Store;

pub const WEB_ATTACHMENT_NAMESPACE: &str = "web.attachment";
const WEB_ATTACHMENT_CODEC: &str = "shenko.web.attachment.bytes";
/// Largest single attachment the browser path accepts.
pub const MAX_WEB_ATTACHMENT_BYTES: u64 = 64 * 1024 * 1024;
/// Most attachments one submission may carry.
pub const MAX_WEB_ATTACHMENTS: usize = 16;

/// One attachment as a submission names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebAttachment {
    pub digest: Digest,
    pub name: String,
    pub media_type: String,
    pub bytes: u64,
}

impl WebAttachment {
    /// Shape checks that need no storage: a usable name, a `type/subtype`
    /// media type and a size within the browser path's bound.
    pub fn validate(&self) -> Result<(), &'static str> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 255 || name.chars().any(char::is_control) {
            return Err("附件名称为空、过长或含控制字符。");
        }
        if !valid_media_type(&self.media_type) {
            return Err("附件媒体类型无效。");
        }
        if self.bytes > MAX_WEB_ATTACHMENT_BYTES {
            return Err("附件超过大小上限。");
        }
        Ok(())
    }

    pub fn is_image(&self) -> bool {
        self.media_type.starts_with("image/")
    }

    /// Text the subject can read inline: declared text-like types only; the
    /// bytes must still decode as UTF-8 when rendered.
    pub fn is_text(&self) -> bool {
        let media = self.media_type.as_str();
        media.starts_with("text/")
            || matches!(
                media,
                "application/json" | "application/xml" | "application/yaml" | "application/x-yaml"
                    | "application/toml" | "application/javascript" | "application/x-sh"
            )
    }
}

fn valid_media_type(value: &str) -> bool {
    let Some((kind, sub)) = value.split_once('/') else { return false };
    let token = |part: &str| {
        !part.is_empty()
            && part.len() <= 127
            && part.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&byte))
    };
    token(kind) && token(sub)
}

/// Normalise a browser-declared media type: drop parameters, lowercase, and
/// fall back to `application/octet-stream` when it is missing or malformed.
pub fn normalise_media_type(declared: Option<&str>) -> String {
    let base = declared
        .and_then(|value| value.split(';').next())
        .map(|value| value.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if valid_media_type(&base) { base } else { "application/octet-stream".to_owned() }
}

impl Store {
    /// Seal uploaded bytes and return the reference a submission may carry.
    pub fn seal_web_attachment(&self, data: &[u8], name: &str, media_type: &str) -> Result<WebAttachment, ShenkoError> {
        let attachment = WebAttachment {
            digest: Digest::blake3(data),
            name: name.trim().to_owned(),
            media_type: media_type.to_owned(),
            bytes: data.len() as u64,
        };
        attachment.validate().map_err(ShenkoError::conflict)?;
        let sealed = self.cas.seal_object(data, media_type, WEB_ATTACHMENT_NAMESPACE, WEB_ATTACHMENT_CODEC, 1)?;
        debug_assert_eq!(sealed.object.digest, attachment.digest);
        Ok(attachment)
    }

    /// Whether the CAS holds exactly the bytes this reference names.
    pub fn web_attachment_present(&self, attachment: &WebAttachment) -> Result<bool, ShenkoError> {
        Ok(self.cas.object_len(&attachment.digest)? == Some(attachment.bytes))
    }

    /// At most `limit` leading bytes of an attachment.
    pub fn web_attachment_prefix(&self, attachment: &WebAttachment, limit: usize) -> Result<Vec<u8>, ShenkoError> {
        self.cas.read_prefix(&attachment.digest, limit)
    }

    /// All bytes of a stored attachment object, for serving it back.
    pub fn web_attachment_bytes(&self, digest: &Digest) -> Result<Vec<u8>, ShenkoError> {
        self.cas.read_object(digest)
    }
}
