//! Conservative retention references, not replication or execution authority.
//! JSON-encoded row cells are followed, opaque CAS payloads are not interpreted.
use std::io::Read;

use anyhow::Result;
use serde_json::{Map, Value};
use shenko_types::object::{Digest, ObjectRef, SchemaRef};

use super::{Reader, RetentionOrigin, RetentionReference, RetentionStorage};

/// Equal bytes do not establish equal interpretations. In particular, a
/// generic JSON walk must not satisfy the later tool-manifest shape reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum JsonReading {
    Generic,
    ToolChunkManifest,
}

/// Only the known attitude-reasons column gives a digest/locator pair its
/// SourceRef interpretation. Identically shaped arbitrary artifact metadata
/// still goes through the conservative artifact reader.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ValueReading {
    Generic,
    Reasons,
    Reason,
    Termination,
    Source,
}

impl ValueReading {
    fn field(self, key: &str) -> Self {
        match (self, key) {
            (Self::Reason, "termination") => Self::Termination,
            (Self::Termination, "source") => Self::Source,
            _ => Self::Generic,
        }
    }
}

pub(super) fn is_json(media_type: &str) -> bool {
    media_type == "application/json" || media_type.ends_with("+json")
}

pub(super) fn digest(text: &str) -> Option<Digest> {
    let digest = Digest(text.into());
    digest.canonical_blake3_hex().map(|_| digest.clone())
}

pub(super) fn short_text(map: &Map<String, Value>, key: &str) -> Option<String> {
    let text = map.get(key)?.as_str()?;
    (text.len() <= 1024).then(|| text.to_owned())
}

pub(super) fn object_reference(map: &Map<String, Value>) -> Option<ObjectRef> {
    let codec = map.get("codec")?.as_object()?;
    map.get("locator")?.as_str()?;
    Some(ObjectRef {
        digest: digest(map.get("digest")?.as_str()?)?,
        bytes: map.get("bytes")?.as_u64()?,
        media_type: short_text(map, "media_type")?,
        store_namespace: short_text(map, "store_namespace")?,
        // A historical locator is neither a read capability nor report data.
        locator: String::new(),
        codec: SchemaRef {
            name: short_text(codec, "name")?,
            major: u32::try_from(codec.get("major")?.as_u64()?).ok()?,
            digest: digest(codec.get("digest")?.as_str()?)?,
        },
    })
}

impl Reader<'_> {
    pub(super) fn required_digest(&mut self, bytes: &[u8], origin: &RetentionOrigin) {
        if let Some(digest) = std::str::from_utf8(bytes).ok().and_then(digest) {
            self.reference(RetentionReference {
                digest, object: None, required: true, storage: RetentionStorage::CasDigest, origin: origin.clone(),
            });
        } else {
            self.gap("invalid_required_digest", origin.clone(), None);
        }
    }

    pub(super) fn reference(&mut self, reference: RetentionReference) {
        if self.report.references.len() >= self.report.limits.references {
            self.gap("reference_budget_exceeded", reference.origin, Some(reference.digest));
            self.exhausted = true;
            return;
        }
        self.queue.push_back(reference.clone());
        self.report.references.push(reference);
    }

    pub(super) fn json(
        &mut self, bytes: &[u8], origin: &RetentionOrigin, required: bool, depth: usize, object: Option<&ObjectRef>,
    ) {
        if self.exhausted { return; }
        if depth > self.report.limits.depth {
            self.gap("json_depth_exceeded", origin.clone(), None);
            return;
        }
        // Parsing is independently bounded, including nested JSON strings.
        let Some(total) = self.report.json_bytes_read.checked_add(bytes.len() as u64)
            .filter(|total| *total <= self.report.limits.json_bytes) else {
                self.gap("json_budget_exceeded", origin.clone(), None);
                self.exhausted = true;
                return;
            };
        self.report.json_bytes_read = total;
        match serde_json::from_slice::<Value>(bytes) {
            Ok(value) => {
                if depth == 0 && !super::carriers::readable_row(&value, origin) {
                    self.gap("invalid_column_carrier", origin.clone(), None);
                }
                if object.is_some_and(|object| object.store_namespace == "wasm-tool-artifact"
                    && (!super::artifacts::chunk_manifest_reference(object)
                        || !super::artifacts::readable_chunk_manifest(&value)))
                {
                    self.gap("unsupported_tool_chunk_manifest", origin.clone(), None);
                }
                let reading = if depth == 0 && matches!(origin,
                    RetentionOrigin::Row { table, column, .. }
                        if table == "cognition_attitudes" && column == "reasons_json")
                {
                    ValueReading::Reasons
                } else {
                    ValueReading::Generic
                };
                self.value(&value, origin, depth, reading);
            }
            Err(_) if required => self.gap("invalid_json_carrier", origin.clone(), None),
            Err(_) => {
                if let Ok(text) = std::str::from_utf8(bytes)
                    && let Some(digest) = digest(text)
                {
                    self.reference(RetentionReference {
                        digest, object: None, required: false, storage: RetentionStorage::OptionalDigest, origin: origin.clone(),
                    });
                }
            }
        }
    }

    fn value(&mut self, value: &Value, origin: &RetentionOrigin, depth: usize, reading: ValueReading) {
        if self.exhausted { return; }
        if depth > self.report.limits.depth {
            self.gap("json_depth_exceeded", origin.clone(), None);
            return;
        }
        match value {
            Value::Object(map) => {
                if map.get("record_schema_version").is_some_and(|version| version.as_u64() != Some(1)) {
                    // Current task/operation/attempt/E1 records all use version
                    // one. Retain visible references, but do not claim that an
                    // unknown record representation has a complete closure.
                    self.gap("unsupported_record_schema_version", origin.clone(), None);
                }
                let source = reading == ValueReading::Source && self.cognition_source(map, origin);
                let artifact = source || self.artifact(map, origin);
                // Missing namespace/codec is not permission to demote an
                // ObjectRef-shaped carrier to a harmless optional digest.
                let looks_typed = !artifact
                    && ((map.contains_key("digest") && (map.contains_key("codec")
                        || map.contains_key("store_namespace")
                        || (map.contains_key("bytes") && map.contains_key("media_type"))))
                        || (map.contains_key("store_namespace") && map.contains_key("codec")));
                if looks_typed {
                    if let Some(object) = object_reference(map) {
                        self.reference(RetentionReference {
                            digest: object.digest.clone(), object: Some(object), required: true,
                            storage: RetentionStorage::CasObject, origin: origin.clone(),
                        });
                    } else {
                        self.gap("invalid_object_reference", origin.clone(), None);
                    }
                    // Even a malformed reference may contain a retained digest.
                    // Descend conservatively, without treating it as a valid proof.
                }
                for (key, nested) in map {
                    // A valid ObjectRef's codec digest is a schema identity, not
                    // an extra content edge. Other fields still support aliases.
                    if (looks_typed && key == "codec") || (artifact && key == "digest")
                        || (source && key == "locator") { continue; }
                    self.value(nested, origin, depth + 1, reading.field(key));
                }
            }
            Value::Array(items) => {
                let item_reading = if reading == ValueReading::Reasons {
                    ValueReading::Reason
                } else {
                    ValueReading::Generic
                };
                for item in items { self.value(item, origin, depth + 1, item_reading); }
            }
            Value::String(text) => {
                if let Some(digest) = digest(text) {
                    self.reference(RetentionReference {
                        digest, object: None, required: false, storage: RetentionStorage::OptionalDigest, origin: origin.clone(),
                    });
                } else if matches!(text.trim_start().as_bytes().first(), Some(b'{' | b'[')) {
                    // cognition_baselines.row_values and canonical Cell::Text
                    // embed original SQL JSON in strings. This retention-only
                    // over-approximation grants no graph/execution authority to
                    // transport, a native receipt reader or opaque CAS content.
                    self.json(text.as_bytes(), origin, true, depth + 1, None);
                }
            }
            _ => {}
        }
    }

    pub(super) fn expand_closure(&mut self) -> Result<()> {
        while let Some(reference) = self.queue.pop_front() {
            if self.exhausted {
                self.gap("closure_not_scanned_after_budget", reference.origin, Some(reference.digest));
                break;
            }
            let expected = reference.object.as_ref().map(|object| object.bytes);
            let reading = reference.object.as_ref().filter(|object| is_json(&object.media_type))
                .map(|object| if object.store_namespace == "wasm-tool-artifact" {
                    JsonReading::ToolChunkManifest
                } else { JsonReading::Generic });
            let json = reading.is_some();
            let origin = RetentionOrigin::Object { digest: reference.digest.clone() };
            // Check every manifest edge, even when these bytes and this reading
            // were seen before through a different (possibly valid) codec.
            if reading == Some(JsonReading::ToolChunkManifest)
                && reference.object.as_ref().is_none_or(|object| !super::artifacts::chunk_manifest_reference(object))
            {
                self.gap("unsupported_tool_manifest_codec", reference.origin.clone(), Some(reference.digest.clone()));
            }
            if let Some(&bytes) = self.measured.get(&reference.digest) {
                if expected.is_some_and(|expected| expected != bytes) {
                    self.gap("object_length_disagrees", reference.origin, Some(reference.digest));
                    continue;
                }
                if !json || reading.is_some_and(|reading| self.expanded.contains(&(reference.digest.clone(), reading))) {
                    continue;
                }
            }
            let mut file = match self.cas.open_retained_object(&reference.digest) {
                Ok(file) => file,
                Err(error) if !reference.required && error.kind() == std::io::ErrorKind::NotFound => {
                    self.report.external_or_absent_digests.insert(reference.digest);
                    continue;
                }
                Err(_) => {
                    self.gap("referenced_object_unreadable", reference.origin, Some(reference.digest));
                    continue;
                }
            };
            let metadata = match file.metadata() {
                Ok(metadata) if metadata.is_file() => metadata,
                _ => {
                    self.gap("referenced_object_not_regular", reference.origin, Some(reference.digest));
                    continue;
                }
            };
            let bytes = metadata.len();
            if expected.is_some_and(|expected| expected != bytes) {
                self.gap("object_length_disagrees", reference.origin, Some(reference.digest));
                continue;
            }
            if json && bytes > self.report.limits.json_bytes.saturating_sub(self.report.json_bytes_read) {
                self.gap("json_budget_exceeded", origin, Some(reference.digest));
                continue;
            }
            if !self.charge(bytes, &origin) { break; }
            let mut buffer = [0u8; 64 * 1024];
            let mut hash = blake3::Hasher::new();
            let mut body = Vec::new();
            let mut read = 0u64;
            let mut failed = false;
            loop {
                // At most the declared/observed size plus one byte, including
                // files concurrently extended after fstat. Never follow locator.
                let wanted = usize::try_from((bytes.saturating_sub(read) + 1).min(buffer.len() as u64))?;
                match file.read(&mut buffer[..wanted]) {
                    Ok(0) => break,
                    Ok(count) => {
                        read += count as u64;
                        if read > bytes { failed = true; break; }
                        hash.update(&buffer[..count]);
                        if json { body.extend_from_slice(&buffer[..count]); }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => { failed = true; break; }
                }
            }
            if failed || read != bytes || Digest(format!("blake3:{}", hash.finalize().to_hex())) != reference.digest {
                self.gap("referenced_object_bytes_differ", reference.origin, Some(reference.digest));
                continue;
            }
            self.measured.insert(reference.digest.clone(), bytes);
            self.report.verified_objects.insert(reference.digest.clone());
            self.report.external_or_absent_digests.remove(&reference.digest);
            if let Some(reading) = reading {
                self.expanded.insert((reference.digest, reading));
                self.json(&body, &origin, true, 0, reference.object.as_ref());
            }
        }
        Ok(())
    }
}
