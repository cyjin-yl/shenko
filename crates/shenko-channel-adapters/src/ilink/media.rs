//! Encrypted media of the WeChat iLink CDN — docs/specs/channel-adapters.md#spec-16, docs/specs/channels.md#spec-15-1.
//!
//! The iLink CDN stores media encrypted with AES-128-ECB and PKCS#7 padding.
//! An upload is therefore four steps:
//!
//! 1. hash the plaintext (MD5) and size it;
//! 2. generate a 16-byte AES key and a 16-byte file key;
//! 3. ask the provider where to upload (`getUploadUrl`);
//! 4. upload the ciphertext and keep the `x-encrypted-param` the CDN returns.
//!
//! The download parameter and the AES key travel with the message as the
//! media reference. The bytes themselves stay on the CDN: an inbound
//! attachment is a remote resource, never a local file.

use serde_json::{Value, json};
use shenko_types::channel::{AttachmentError, MAX_ATTACHMENT_BYTES};

use crate::aes;

use super::api::{ILinkApi, ILinkError, MediaKind};

/// The encrypted media reference the provider understands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedMedia {
    /// The download parameter the CDN issued.
    pub encrypt_query_param: String,
    /// The AES key, base64 of the hexadecimal key string.
    pub aes_key: String,
    /// The encryption kind; `1` is AES-128-ECB.
    pub encrypt_type: u8,
    /// The complete download URL, when the provider published one.
    pub full_url: Option<String>,
}

impl EncryptedMedia {
    /// The media reference as the provider expects it inside a message item.
    pub fn as_reference(&self) -> Value {
        let mut reference = json!({
            "encrypt_query_param": self.encrypt_query_param,
            "aes_key": self.aes_key,
            "encrypt_type": self.encrypt_type,
        });
        if let Some(url) = &self.full_url {
            reference["full_url"] = Value::String(url.clone());
        }
        reference
    }

    /// Read a media reference out of a provider payload.
    pub fn from_value(value: &Value) -> Option<Self> {
        let encrypt_query_param = value
            .get("encrypt_query_param")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())?
            .to_string();
        let aes_key = value
            .get("aes_key")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())?
            .to_string();
        let encrypt_type = value
            .get("encrypt_type")
            .and_then(Value::as_i64)
            .unwrap_or(1) as u8;
        let full_url = value
            .get("full_url")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned);
        Some(Self {
            encrypt_query_param,
            aes_key,
            encrypt_type,
            full_url,
        })
    }
}

/// What one upload produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedFile {
    /// The media reference to put into the message item.
    pub media: EncryptedMedia,
    /// The client-generated file key.
    pub filekey: String,
    /// The plaintext size in bytes.
    pub raw_size: usize,
    /// The ciphertext size in bytes.
    pub cipher_size: usize,
    /// The AES key in hexadecimal.
    pub aes_key_hex: String,
}

/// Upload bytes to the iLink CDN and return the media reference.
pub async fn upload(
    api: &ILinkApi,
    to_user_id: &str,
    kind: MediaKind,
    bytes: &[u8],
) -> Result<UploadedFile, ILinkError> {
    let raw_size = bytes.len();
    let raw_md5 = md5_hex(bytes);
    let cipher_size = padded_size(raw_size);
    let entropy = |what: &str| {
        random_bytes(16).map_err(|error| ILinkError::Incomplete(format!("{what}: {error}")))
    };
    let filekey = hex::encode(entropy("filekey entropy")?);
    let aes_key = entropy("aes key entropy")?;
    let aes_key_hex = hex::encode(&aes_key);

    let ticket = api
        .get_upload_url(
            &filekey,
            kind,
            to_user_id,
            raw_size,
            &raw_md5,
            cipher_size,
            &aes_key_hex,
        )
        .await?;
    let upload_full_url = ticket
        .get("upload_full_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_owned);
    let upload_param = ticket
        .get("upload_param")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|param| !param.is_empty())
        .map(str::to_owned);
    if upload_full_url.is_none() && upload_param.is_none() {
        return Err(ILinkError::Incomplete(
            "getUploadUrl returned neither upload_full_url nor upload_param".to_string(),
        ));
    }

    let ciphertext = aes::encrypt_ecb_pkcs7(&aes_key, bytes).map_err(|error| {
        ILinkError::Incomplete(format!("media encryption failed: {error}"))
    })?;
    let target = match upload_full_url {
        Some(url) => url,
        None => {
            let param = upload_param.expect("checked above");
            format!(
                "{}/upload?encrypted_query_param={}&filekey={}",
                api.config().cdn_base_url(),
                encode_query(&param),
                encode_query(&filekey)
            )
        }
    };
    let encrypt_query_param = api.upload_cdn(&target, ciphertext).await?;
    Ok(UploadedFile {
        media: EncryptedMedia {
            encrypt_query_param,
            // The provider's own sender base64-encodes the hexadecimal key
            // string; the decoder accepts that form and the raw 16 bytes.
            aes_key: base64_encode(aes_key_hex.as_bytes()),
            encrypt_type: 1,
            full_url: None,
        },
        filekey,
        raw_size,
        cipher_size,
        aes_key_hex,
    })
}

/// Download and decrypt one media object under the channel byte bound.
///
/// The ciphertext is fetched in provider chunks with the same hard bound as
/// every other channel (one AES block of padding is allowed on top), and the
/// decrypted plaintext is checked again before it is handed on. A CDN that
/// refuses the object keeps the platform's own answer: a refused credential is
/// not the same fact as a missing object, and neither is reported as empty
/// bytes. The declared size is deliberately *not* enforced here: for iLink the
/// size field differs by item kind (`len` / `mid_size` / `video_size`) and its
/// meaning is not the plaintext length for every kind, so a mismatch would be a
/// fabricated verdict rather than an integrity failure.
pub async fn download(
    api: &ILinkApi,
    media: &EncryptedMedia,
    attachment_id: &str,
) -> Result<Vec<u8>, AttachmentError> {
    if media.encrypt_type != 1 {
        return Err(AttachmentError::Refused {
            attachment_id: attachment_id.to_string(),
            detail: format!(
                "media encryption kind {} is not AES-128-ECB (1)",
                media.encrypt_type
            ),
        });
    }
    let url = match &media.full_url {
        Some(url) if !url.trim().is_empty() => url.clone(),
        _ => api.download_url(&media.encrypt_query_param),
    };
    let ciphertext = match crate::common::fetch_media_bounded(api.http(), &url, None, attachment_id)
        .await?
    {
        crate::common::MediaAnswer::Delivered { bytes, .. } => bytes,
        crate::common::MediaAnswer::Refused { status, detail } => {
            return Err(crate::common::refused_attachment(
                super::events::ILINK_CHANNEL_ID,
                attachment_id,
                status,
                detail,
            ))
        }
    };
    // A fetch that already saturated the raw bound cannot be a padded block
    // sequence of a legal plaintext.
    if ciphertext.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(AttachmentError::TooLarge {
            attachment_id: attachment_id.to_string(),
            limit: MAX_ATTACHMENT_BYTES,
        });
    }
    let key = decode_key(&media.aes_key).map_err(|detail| AttachmentError::Refused {
        attachment_id: attachment_id.to_string(),
        detail: format!("media key refused: {detail}"),
    })?;
    let plaintext = aes::decrypt_ecb_pkcs7(&key, &ciphertext).map_err(|error| {
        AttachmentError::Unavailable {
            attachment_id: attachment_id.to_string(),
            detail: format!("media decryption failed: {error}"),
        }
    })?;
    if plaintext.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(AttachmentError::TooLarge {
            attachment_id: attachment_id.to_string(),
            limit: MAX_ATTACHMENT_BYTES,
        });
    }
    Ok(plaintext)
}

/// Decode the AES key of a media reference.
///
/// The provider accepts base64 of sixteen raw bytes and base64 of a
/// thirty-two character hexadecimal key; both forms are accepted here.
pub fn decode_key(key: &str) -> Result<[u8; 16], String> {
    use base64::Engine as _;
    if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(key) {
        if decoded.len() == 16 {
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&decoded);
            return Ok(bytes);
        }
        if let Ok(text) = String::from_utf8(decoded) {
            if text.len() == 32 {
                if let Ok(bytes) = hex::decode(&text) {
                    let mut key = [0u8; 16];
                    key.copy_from_slice(&bytes);
                    return Ok(key);
                }
            }
        }
    }
    if key.len() == 32 {
        if let Ok(bytes) = hex::decode(key) {
            let mut material = [0u8; 16];
            material.copy_from_slice(&bytes);
            return Ok(material);
        }
    }
    Err("media aes_key is neither 16 raw bytes nor a 32 character hex key".to_string())
}

/// The ciphertext size of a plaintext of this size.
pub fn padded_size(plaintext_size: usize) -> usize {
    plaintext_size.div_ceil(16) * 16 + if plaintext_size % 16 == 0 { 16 } else { 0 }
}

/// The MD5 of the bytes, lowercase hexadecimal.
pub fn md5_hex(bytes: &[u8]) -> String {
    format!("{:x}", md5::compute(bytes))
}

/// Standard base64 with padding.
fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Random bytes from the operating system's entropy source.
pub fn random_bytes(count: usize) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let mut buffer = vec![0u8; count];
    let mut file = std::fs::File::open("/dev/urandom")
        .map_err(|error| format!("entropy source unavailable: {error}"))?;
    file.read_exact(&mut buffer)
        .map_err(|error| format!("entropy read failed: {error}"))?;
    Ok(buffer)
}

/// Percent-encode a query parameter.
fn encode_query(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}
