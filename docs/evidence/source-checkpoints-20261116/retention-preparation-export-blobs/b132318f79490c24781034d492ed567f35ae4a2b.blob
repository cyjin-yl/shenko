//! Persisted artifact storage shapes, not executable admission. ToolArtifactRef
//! is owned by shenko-wasm; this reader only interprets its storage description
//! and follows the real manifest ObjectRef, without depending on that runtime.
use serde_json::{Map, Value};

use super::{Reader, RetentionOrigin, RetentionReference, RetentionStorage};
use super::closure::{digest, is_json, object_reference, short_text};

/// The exact portable suffix produced by ContentStore, without allocating
/// path components or opening any part of the historical path.
fn cas_locator(locator: &str) -> Option<(&str, &str, &str)> {
    let mut parts = locator.strip_prefix("file://")?.rsplit('/');
    let hex = parts.next()?;
    let shard = parts.next()?;
    let parent = parts.next()?;
    (hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && shard.len() == 2 && parent.len() == 2).then_some((parent, shard, hex))
}

fn tool_shape(map: &Map<String, Value>) -> bool {
    map.keys().all(|key| matches!(key.as_str(),
        "kind" | "digest" | "bytes" | "media_type" | "store_namespace" | "sha256" | "chunk_manifest"))
        && map.get("bytes").and_then(Value::as_u64).is_some()
        && short_text(map, "media_type").is_some()
        && short_text(map, "store_namespace").is_some()
        && map.get("sha256").is_none_or(|value| value.is_null() || value.as_str().is_some())
}

/// This namespace's JSON objects are the current schema-1 chunk manifests.
/// A syntactically valid JSON scalar cannot silently stand in for its edges.
pub(super) fn readable_chunk_manifest(value: &Value) -> bool {
    let Some(map) = value.as_object() else { return false; };
    let Some(objects) = map.get("objects").and_then(Value::as_array) else { return false; };
    map.keys().all(|key| matches!(key.as_str(),
        "schema" | "artifact_digest" | "bytes" | "chunks" | "objects" | "sha256"))
        && map.get("schema").and_then(Value::as_u64) == Some(1)
        && map.get("artifact_digest").and_then(Value::as_str).and_then(digest).is_some()
        && map.get("bytes").and_then(Value::as_u64).is_some()
        && map.get("chunks").and_then(Value::as_u64) == Some(objects.len() as u64)
        && objects.iter().all(|value| value.as_object().and_then(object_reference).is_some())
        && map.get("sha256").is_none_or(|value| value.is_null() || value.as_str().is_some())
}

impl Reader<'_> {
    pub(super) fn memory_locator(&mut self, bytes: &[u8], origin: &RetentionOrigin) {
        if let Ok(locator) = std::str::from_utf8(bytes)
            && let Some((parent, shard, hex)) = cas_locator(locator)
            && parent == &hex[..2] && shard == &hex[2..4]
        {
            self.reference(RetentionReference {
                digest: shenko_types::object::Digest(format!("blake3:{hex}")), object: None,
                required: true, storage: RetentionStorage::CasArtifact, origin: origin.clone(),
            });
        } else {
            // Historical memory may have only this String, without the newer
            // full memory_intent_bindings ObjectRef. Do not silently drop it.
            self.gap("unclassified_memory_content_locator", origin.clone(), None);
        }
    }

    /// Return whether this map is an artifact/source descriptor, so it is not
    /// also misread as an ObjectRef just because it has namespace and digest.
    pub(super) fn artifact(&mut self, map: &Map<String, Value>, origin: &RetentionOrigin) -> bool {
        if !map.contains_key("codec") && map.contains_key("kind")
            && map.contains_key("store_namespace") && map.contains_key("digest")
        {
            self.tool_artifact(map, origin);
            return true;
        }
        if map.contains_key("digest") && map.contains_key("locator")
            && !["codec", "bytes", "media_type", "store_namespace"].iter().any(|key| map.contains_key(*key))
        {
            self.located_artifact(map, origin);
            return true;
        }
        false
    }

    fn tool_artifact(&mut self, map: &Map<String, Value>, origin: &RetentionOrigin) {
        let Some(identity) = map.get("digest").and_then(Value::as_str).and_then(digest) else {
            self.gap("invalid_tool_artifact_digest", origin.clone(), None);
            return;
        };
        let storage = match map.get("kind").and_then(Value::as_str) {
            Some("content_store") => {
                let manifest = map.get("chunk_manifest").and_then(Value::as_object).and_then(object_reference);
                if manifest.as_ref().is_none_or(|object| !is_json(&object.media_type)) {
                    self.gap("invalid_tool_chunk_manifest_reference", origin.clone(), Some(identity.clone()));
                }
                RetentionStorage::ManifestAssembledArtifact
            }
            Some("node_inline") => {
                if map.get("chunk_manifest").is_some_and(|value| !value.is_null()) {
                    self.gap("unexpected_inline_artifact_manifest", origin.clone(), Some(identity.clone()));
                }
                RetentionStorage::NodeInlineArtifact
            }
            _ => {
                self.gap("unsupported_tool_artifact_storage", origin.clone(), Some(identity.clone()));
                RetentionStorage::OptionalDigest
            }
        };
        if !tool_shape(map) {
            self.gap("invalid_tool_artifact_descriptor", origin.clone(), Some(identity.clone()));
        }
        // The whole-module digest is an identity, not a promised local object.
        // The caller still descends into the actual chunk_manifest reference.
        self.reference(RetentionReference {
            digest: identity, object: None, required: false, storage, origin: origin.clone(),
        });
    }

    fn located_artifact(&mut self, map: &Map<String, Value>, origin: &RetentionOrigin) {
        let Some(identity) = map.get("digest").and_then(Value::as_str).and_then(digest) else {
            self.gap("invalid_artifact_digest", origin.clone(), None);
            return;
        };
        // SourceRef shares digest/locator with ArtifactRef but permits null.
        // Full Task/Operation rows separately decode their non-null ArtifactRef.
        let (storage, required) = match map.get("locator") {
            Some(Value::String(locator)) => self.artifact_location(locator, &identity, origin),
            Some(Value::Null) => (RetentionStorage::OptionalDigest, false),
            _ => {
                self.gap("invalid_artifact_locator", origin.clone(), Some(identity.clone()));
                (RetentionStorage::OptionalDigest, false)
            }
        };
        self.reference(RetentionReference {
            digest: identity, object: None, required, storage, origin: origin.clone(),
        });
    }

    fn artifact_location(
        &mut self, locator: &str, identity: &shenko_types::object::Digest, origin: &RetentionOrigin,
    ) -> (RetentionStorage, bool) {
        if locator.starts_with("file://") {
            if let Some((parent, shard, hex)) = cas_locator(locator) {
                if hex == identity.hex() && parent == &hex[..2] && shard == &hex[2..4] {
                    return (RetentionStorage::CasArtifact, true);
                }
                self.gap("artifact_locator_digest_disagrees", origin.clone(), Some(identity.clone()));
                return (RetentionStorage::OptionalDigest, false);
            }
            return (RetentionStorage::ExternalArtifact, false);
        }
        if ["builtin:", "native:", "channel:", "http://", "https://", "provider://"]
            .iter().any(|prefix| locator.starts_with(prefix))
        {
            return (RetentionStorage::ExternalArtifact, false);
        }
        // No location is read, normalized into a new authority or guessed.
        self.gap("unclassified_artifact_location", origin.clone(), Some(identity.clone()));
        (RetentionStorage::OptionalDigest, false)
    }
}
