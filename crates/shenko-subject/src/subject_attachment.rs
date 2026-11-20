//! Request-local projection of original, admitted browser attachment objects.
//! No URLs, conversions, new CAS objects, or inference outside subject S0.
use std::{collections::HashMap, sync::Arc};

use shenko_broker::request::InferencePart;
use shenko_context_policy::{ItemRef, ModelCaps};
use shenko_store::{Store, web_attachment::WebAttachment};
use shenko_types::{error::ShenkoError, ids::EntityId, object::Digest};

pub(super) struct AttachmentProjection<'a> {
    store: &'a Store,
    scope: EntityId,
    caps: Option<&'a ModelCaps>,
    // A repeated source object shares its original bytes. Every actual image
    // occurrence is charged; metadata references alone never charge an image.
    images: HashMap<Digest, Option<(Arc<[u8]>, u32, u32)>>,
    lengths: HashMap<Digest, u64>,
    supplied: std::collections::HashSet<Digest>,
}

impl<'a> AttachmentProjection<'a> {
    pub(super) fn new(store: &'a Store, scope: EntityId, caps: Option<&'a ModelCaps>) -> Self {
        Self { store, scope, caps, images: HashMap::new(), lengths: HashMap::new(), supplied: Default::default() }
    }

    pub(super) fn project(&mut self, at: ItemRef) -> Result<(Vec<InferencePart>, u32), ShenkoError> {
        let record = self.store.branch_history_event(self.scope, at.event_id)?
            .ok_or_else(|| ShenkoError::conflict("attachment projection raw source missing"))?;
        if u64::try_from(record.seq).ok() != Some(at.seq) {
            return Err(ShenkoError::conflict("attachment projection raw source sequence differs"));
        }
        // StoreHistory deliberately preserves non-JSON user rows as text.
        // They have no typed inbox original to interpret as an attachment.
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&record.data) else {
            return Ok((Vec::new(), 0));
        };
        // Ordinary history and other ingress never become attachment inputs.
        let Some(original) = data.get("inbox_event") else { return Ok((Vec::new(), 0)); };
        let event: shenko_types::inbox::InboxEvent = serde_json::from_value(original.clone())?;
        if event.source != shenko_store::web_input::WEB_INPUT_SOURCE {
            return Ok((Vec::new(), 0));
        }
        let references = match event.payload.get("attachment_refs") {
            None => return Ok((Vec::new(), 0)),
            Some(serde_json::Value::Array(references)) => references,
            Some(_) => return Err(ShenkoError::conflict("typed web attachment references must be an array")),
        };
        if references.len() > shenko_store::web_attachment::MAX_WEB_ATTACHMENTS {
            return Err(ShenkoError::conflict("typed web attachment count exceeds admission bound"));
        }
        let mut parts = Vec::new();
        let mut tokens = 0_u32;
        for reference in references {
            let attachment: WebAttachment = serde_json::from_value(reference.clone())?;
            attachment.validate().map_err(ShenkoError::conflict)?;
            // Cache only an identity whose original CAS object exists at the
            // declared length. This stat-sized check precedes all fallbacks;
            // whole bytes and their digest are checked only for actual images.
            match self.lengths.entry(attachment.digest.clone()) {
                std::collections::hash_map::Entry::Occupied(previous) => {
                    if *previous.get() != attachment.bytes {
                        return Err(ShenkoError::conflict("repeated attachment has conflicting admitted length"));
                    }
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    if !self.store.web_attachment_present(&attachment)? {
                        return Err(ShenkoError::conflict("attachment original missing or differs from admitted length"));
                    }
                    entry.insert(attachment.bytes);
                }
            }
            if !attachment.is_image() { continue; }
            let Some(vision) = self.caps.and_then(|caps| caps.vision)
                .filter(|vision| vision.patch_px > 0 && vision.max_image_context > 0) else {
                parts.push(InferencePart::Text { text: format!(
                    "[本轮附件 {}] 未观察到可用的视觉能力/图像预算；只提供原始附件文字信息，看不到画面。", attachment.digest.0) });
                continue;
            };
            // Vision metadata does not declare arbitrary MIME support. PNG is
            // the existing typed frame format; other declared rasters remain
            // explicitly unvalidated until a decoder/capability contract exists.
            if attachment.media_type != "image/png" {
                parts.push(InferencePart::Text { text: format!(
                    "[本轮附件 {}] {} 未经当前 PNG 验证路径验证，未传递画面，看不到画面。", attachment.digest.0, attachment.media_type) });
                continue;
            }
            if !self.images.contains_key(&attachment.digest) {
                let bytes = self.store.web_attachment_bytes(&attachment.digest)?;
                if bytes.len() as u64 != attachment.bytes || Digest::blake3(&bytes) != attachment.digest {
                    return Err(ShenkoError::conflict("attachment bytes differ from admitted digest/length"));
                }
                let image = validated_png(&bytes).map(|(width, height)| (Arc::from(bytes), width, height));
                self.images.insert(attachment.digest.clone(), image);
            }
            let Some((bytes, width, height)) = self.images[&attachment.digest].as_ref() else {
                parts.push(InferencePart::Text { text: format!(
                    "[本轮附件 {}] 声明为 PNG，但未通过有界静态栅格解码验证；未传递画面，看不到画面。", attachment.digest.0) });
                continue;
            };
            if !self.supplied.insert(attachment.digest.clone()) {
                parts.push(InferencePart::Text { text: format!(
                    "[本轮附件 {}] 同一原始图像已在本请求中提供；此引用不重复发送或计费。", attachment.digest.0) });
                continue;
            }
            let charge = width.div_ceil(vision.patch_px)
                .checked_mul(height.div_ceil(vision.patch_px))
                .ok_or_else(|| ShenkoError::conflict("attachment image charge overflow"))?;
            tokens = tokens.checked_add(charge)
                .ok_or_else(|| ShenkoError::conflict("attachment total image charge overflow"))?;
            parts.push(InferencePart::Text { text: format!(
                "[本轮附件 {}] 原始 PNG 字节 {}×{}，图像输入如下。历史文字中的推理限制只描述当时的投影。",
                attachment.digest.0, width, height) });
            parts.push(InferencePart::Image { media_type: attachment.media_type, bytes: Arc::clone(bytes) });
        }
        Ok((parts, tokens))
    }
}

fn validated_png(bytes: &[u8]) -> Option<(u32, u32)> {
    // Limit both decoder-owned memory and the decoded raster. Admission's
    // compressed-byte bound alone cannot bound decompression allocations.
    let limit = shenko_store::web_attachment::MAX_WEB_ATTACHMENT_BYTES as usize;
    // png 0.17's row-drain inflater does not hard-reject excess IDAT output.
    // Independently enforce the exact scanline count BEFORE giving it input.
    bounded_png_image_data(bytes, limit as u64)?;
    let mut options = png::DecodeOptions::default();
    options.set_ignore_checksums(false);
    options.set_skip_ancillary_crc_failures(false);
    let mut decoder = png::Decoder::new_with_options(std::io::Cursor::new(bytes), options);
    decoder.set_limits(png::Limits { bytes: limit });
    let mut reader = decoder.read_info().ok()?;
    let info = reader.info();
    if info.width == 0 || info.height == 0 || info.animation_control.is_some() { return None; }
    let dimensions = (info.width, info.height);
    let size = reader.output_buffer_size();
    if size > limit { return None; }
    // Consume and validate all rows without retaining an unused decoded copy.
    while reader.next_row().ok()?.is_some() {}
    // Row completion alone does not validate IEND or post-IDAT chunk CRCs.
    reader.finish().ok()?;
    Some(dimensions)
}

fn bounded_png_image_data(bytes: &[u8], limit: u64) -> Option<()> {
    if bytes.get(..8)? != b"\x89PNG\r\n\x1a\n" { return None; }
    let mut offset = 8_usize;
    let mut expected = None;
    let mut inflater = flate2::Decompress::new(true);
    let mut ended = false;
    let mut metadata_output = 0_u64;
    while offset < bytes.len() {
        let header = bytes.get(offset..offset.checked_add(8)?)?;
        let length = u32::from_be_bytes(header[..4].try_into().ok()?) as usize;
        let kind = &header[4..8];
        let start = offset.checked_add(8)?;
        let end = start.checked_add(length)?;
        let data = bytes.get(start..end)?;
        offset = end.checked_add(4)?;
        bytes.get(end..offset)?; // Every chunk must include its CRC.
        if expected.is_none() {
            if kind != b"IHDR" { return None; }
            expected = Some(png_scanline_bytes(data, limit)?);
        } else if kind == b"IHDR" || kind == b"acTL" || kind == b"fcTL" || kind == b"fdAT" {
            return None;
        }
        if kind == b"IDAT" {
            let expected = expected?;
            if ended {
                if !data.is_empty() { return None; }
            } else {
                ended = inflate_chunk(&mut inflater, data, expected)?;
                if ended && inflater.total_out() != expected { return None; }
            }
        }
        // Text/profile streams also reach the full PNG decoder. Admit their
        // actual output under the same aggregate bound before that decoder
        // can inflate them, including chunks appearing after IDAT.
        let compressed = match kind {
            b"iCCP" | b"zTXt" => {
                let separator = data.iter().position(|byte| *byte == 0)?;
                if separator == 0 || separator > 79 || *data.get(separator + 1)? != 0 { return None; }
                Some(data.get(separator + 2..)?)
            }
            b"iTXt" => {
                let separator = data.iter().position(|byte| *byte == 0)?;
                if separator == 0 || separator > 79 { return None; }
                let flag = *data.get(separator + 1)?;
                if flag > 1 || *data.get(separator + 2)? != 0 { return None; }
                let language = data.get(separator + 3..)?;
                let translated = language.get(language.iter().position(|byte| *byte == 0)? + 1..)?;
                let text = translated.get(translated.iter().position(|byte| *byte == 0)? + 1..)?;
                (flag == 1).then_some(text)
            }
            _ => None,
        };
        if let Some(compressed) = compressed {
            let remaining = limit.checked_sub(expected?)?.checked_sub(metadata_output)?;
            let mut metadata = flate2::Decompress::new(true);
            if !inflate_chunk(&mut metadata, compressed, remaining)? { return None; }
            metadata_output = metadata_output.checked_add(metadata.total_out())?;
        }
        if kind == b"IEND" {
            return (data.is_empty() && offset == bytes.len() && ended).then_some(());
        }
    }
    None
}

/// A caller-owned output buffer is the hard boundary, including one sentinel
/// byte for overflow detection. False means another IDAT chunk is required.
fn inflate_chunk(inflater: &mut flate2::Decompress, mut input: &[u8], maximum: u64) -> Option<bool> {
    let mut output = [0_u8; 8192];
    loop {
        let before_in = inflater.total_in();
        let before_out = inflater.total_out();
        let available = maximum.checked_sub(before_out)?.checked_add(1)?;
        let capacity = output.len().min(usize::try_from(available).ok()?);
        let status = inflater.decompress(input, &mut output[..capacity], flate2::FlushDecompress::None).ok()?;
        let consumed = usize::try_from(inflater.total_in().checked_sub(before_in)?).ok()?;
        input = input.get(consumed..)?;
        if inflater.total_out() > maximum { return None; }
        if status == flate2::Status::StreamEnd { return input.is_empty().then_some(true); }
        if consumed == 0 && inflater.total_out() == before_out {
            return input.is_empty().then_some(false);
        }
    }
}

fn png_scanline_bytes(header: &[u8], limit: u64) -> Option<u64> {
    if header.len() != 13 || header[10] != 0 || header[11] != 0 { return None; }
    let width = u64::from(u32::from_be_bytes(header[..4].try_into().ok()?));
    let height = u64::from(u32::from_be_bytes(header[4..8].try_into().ok()?));
    if width == 0 || height == 0 { return None; }
    let depth = header[8];
    let channels = match (header[9], depth) {
        (0, 1 | 2 | 4 | 8 | 16) => 1_u64,
        (2, 8 | 16) => 3,
        (3, 1 | 2 | 4 | 8) => 1,
        (4, 8 | 16) => 2,
        (6, 8 | 16) => 4,
        _ => return None,
    };
    let bits = channels.checked_mul(u64::from(depth))?;
    let passes: &[(u64, u64, u64, u64)] = match header[12] {
        0 => &[(0, 0, 1, 1)],
        1 => &[(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8),
            (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)],
        _ => return None,
    };
    let mut total = 0_u64;
    for &(x, y, dx, dy) in passes {
        if width <= x || height <= y { continue; }
        let columns = (width - x).div_ceil(dx);
        let rows = (height - y).div_ceil(dy);
        let scanline = columns.checked_mul(bits)?.div_ceil(8).checked_add(1)?;
        total = total.checked_add(scanline.checked_mul(rows)?)?;
        if total > limit { return None; }
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::{bounded_png_image_data, validated_png};
    use std::io::Write as _;

    #[test]
    fn subject_attachment_adam7_idat_split_and_aggregate_metadata_boundaries() {
        fn compressed(bytes: &[u8]) -> Vec<u8> {
            let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(bytes).unwrap();
            encoder.finish().unwrap()
        }
        fn chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            output.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
            output.extend_from_slice(kind);
            output.extend_from_slice(data);
            let mut crc = crc32fast::Hasher::new();
            crc.update(kind);
            crc.update(data);
            output.extend_from_slice(&crc.finalize().to_be_bytes());
        }
        // Packed grayscale, including empty Adam7 passes. These literal
        // filtered rows are independent of the production scanline formula:
        // 1x1 has one two-byte row; 3x2 has four two-byte pass rows.
        for (width, height, filtered) in [(1_u32, 1_u32, &[0_u8; 2][..]), (3, 2, &[0_u8; 8][..])] {
            for after_idat in [false, true] {
                for metadata_count in [1, 2] {
                    let mut header = Vec::new();
                    header.extend_from_slice(&width.to_be_bytes());
                    header.extend_from_slice(&height.to_be_bytes());
                    header.extend_from_slice(&[1, 0, 0, 0, 1]);
                    let mut text = b"key\0\0".to_vec();
                    text.extend_from_slice(&compressed(b"abcde"));
                    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
                    chunk(&mut png, b"IHDR", &header);
                    if !after_idat {
                        for _ in 0..metadata_count { chunk(&mut png, b"zTXt", &text); }
                    }
                    // Empty IDAT plus single-byte boundaries exercise zlib
                    // header, body and checksum continuation independently.
                    chunk(&mut png, b"IDAT", &[]);
                    for byte in compressed(filtered).chunks(1) { chunk(&mut png, b"IDAT", byte); }
                    chunk(&mut png, b"IDAT", &[]);
                    if after_idat {
                        for _ in 0..metadata_count { chunk(&mut png, b"zTXt", &text); }
                    }
                    chunk(&mut png, b"IEND", &[]);
                    let exact = filtered.len() as u64 + metadata_count * 5;
                    assert_eq!(bounded_png_image_data(&png, exact), Some(()));
                    assert_eq!(bounded_png_image_data(&png, exact - 1), None);
                    assert_eq!(bounded_png_image_data(&png, exact + 1), Some(()));
                    assert_eq!(validated_png(&png), Some((width, height)));
                }
            }
        }
    }
}
