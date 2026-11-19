//! Typed CAS reference discovery and complete recovery-object verification.
//!
//! Opaque content never gains graph authority by looking like JSON. Export
//! and admission share the same treatment of typed aliases and raw digests.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use shenko_types::object::{Digest, ObjectRef, SchemaRef};

use crate::cognition_archive::{
    CognitionGap, CognitionManifest, CognitionObjectDescriptor, CognitionRow,
};
use crate::store::Store;

impl Store {
    pub(crate) fn cognition_object_roots(&self, rows: &[CognitionRow]) -> (Vec<ObjectRef>, Vec<Digest>) {
        self.cognition_text_object_roots(rows.iter().flat_map(|row| {
            row.values.iter().filter_map(move |(name, value)| {
                value.as_deref().map(|value| (row.table.as_str(), name.as_str(), value))
            })
        }))
    }

    /// The same reference semantics over borrowed SQL text. A canonical row
    /// capture already owns its bounded cells; constructing a second complete
    /// CognitionRow copy solely to discover their roots would duplicate them.
    pub(crate) fn cognition_text_object_roots<'a>(
        &self,
        values: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
    ) -> (Vec<ObjectRef>, Vec<Digest>) {
        let mut roots: Vec<ObjectRef> = Vec::new();
        let mut external: Vec<Digest> = Vec::new();
        for (table, name, value) in values {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(value) {
                collect_references(&json, &mut roots, &mut external);
            } else if table == "context_notes" && name == "body" {
                // An authored note is free text. A body that is a
                // bare digest is read as a possible external
                // reference — a non-blocking reading, never a reason
                // to drop or rewrite the note's bytes.
                if let Some(digest) = parse_digest(value) {
                    external.push(digest);
                }
            }
        }
        for digest in &external {
            if let Some(object) = self.bare_object(digest) {
                roots.push(object);
            }
        }
        (roots, external)
    }

    /// The verified CAS closure of the given roots, recursively.
    pub(crate) fn collect_closure(
        &self,
        roots: &[ObjectRef],
    ) -> Result<(Vec<CognitionObjectDescriptor>, Vec<Digest>)> {
        self.collect_closure_bounded(roots, usize::MAX, u64::MAX)
    }

    /// A factual transport can impose its own finite inventory budget without
    /// changing the closure semantics of existing cognition archives.
    pub(crate) fn collect_closure_bounded(
        &self,
        roots: &[ObjectRef],
        max_objects: usize,
        max_bytes: u64,
    ) -> Result<(Vec<CognitionObjectDescriptor>, Vec<Digest>)> {
        // Content membership and JSON expansion are different facts. One
        // digest can have both opaque and JSON-typed references; encountering
        // the opaque alias first must not hide a later typed child closure.
        let mut collected: BTreeMap<String, usize> = BTreeMap::new();
        let mut expanded: BTreeSet<String> = BTreeSet::new();
        let mut missing: Vec<Digest> = Vec::new();
        let mut out: Vec<CognitionObjectDescriptor> = Vec::new();
        let mut total_bytes = 0u64;
        let mut queue: std::collections::VecDeque<ObjectRef> = roots.iter().cloned().collect();
        while let Some(object) = queue.pop_front() {
            anyhow::ensure!(queue.len() < max_objects, "CAS closure reference budget exceeded");
            let key = object.digest.to_string();
            let expand = is_json_reference(&object.media_type) && !expanded.contains(&key);
            if let Some(&index) = collected.get(&key) {
                if out[index].bytes != object.bytes {
                    missing.push(object.digest.clone());
                    continue;
                }
                if !expand {
                    continue;
                }
            } else {
                anyhow::ensure!(out.len() < max_objects, "CAS closure object budget exceeded");
                total_bytes = total_bytes.checked_add(object.bytes)
                    .filter(|total| *total <= max_bytes)
                    .ok_or_else(|| anyhow::anyhow!("CAS closure byte budget exceeded"))?;
            }
            let bytes = match self.cas.read_object(&object.digest) {
                Ok(bytes) => bytes,
                Err(_) => {
                    missing.push(object.digest.clone());
                    continue;
                }
            };
            if bytes.len() as u64 != object.bytes || Digest::blake3(&bytes) != object.digest {
                missing.push(object.digest.clone());
                continue;
            }
            let descriptor = CognitionObjectDescriptor {
                digest: object.digest.clone(),
                bytes: object.bytes,
                media_type: object.media_type.clone(),
                store_namespace: object.store_namespace.clone(),
                codec: object.codec.clone(),
            };
            if let Some(&index) = collected.get(&key) {
                // Preserve the later typed declaration in the manifest too,
                // so the receiver sees the same expansion authority.
                out[index] = descriptor;
            } else {
                collected.insert(key.clone(), out.len());
                out.push(descriptor);
            }
            if expand {
                expanded.insert(key);
                if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    let mut nested: Vec<ObjectRef> = Vec::new();
                    let mut external: Vec<Digest> = Vec::new();
                    collect_references(&json, &mut nested, &mut external);
                    queue.extend(nested);
                    for digest in external {
                        if let Some(bare) = self.bare_object(&digest) {
                            queue.push_back(bare);
                        }
                    }
                }
            }
        }
        Ok((out, missing))
    }

    /// A digest that exists in this store's CAS, seen as a plain object.
    fn bare_object(&self, digest: &Digest) -> Option<ObjectRef> {
        if !self.cas.object_exists(digest) {
            return None;
        }
        let bytes = self.cas.object_size(digest)?;
        Some(ObjectRef {
            digest: digest.clone(),
            bytes,
            media_type: "application/octet-stream".into(),
            store_namespace: "replicated".into(),
            locator: String::new(),
            codec: raw_codec(),
        })
    }

    /// Check on the receiving side that every referenced object really is in
    /// the archive, including the objects referenced by other objects.
    pub(super) fn verify_manifest_closure(&self, manifest: &CognitionManifest) -> Vec<CognitionGap> {
        let listed: BTreeSet<String> = manifest
            .objects
            .iter()
            .map(|descriptor| descriptor.digest.to_string())
            .collect();
        let mut gaps: Vec<CognitionGap> = Vec::new();
        let mut roots: Vec<ObjectRef> = Vec::new();
        let mut _external: Vec<Digest> = Vec::new();
        for row in &manifest.rows {
            for (_, value) in &row.values {
                if let Some(value) = value.as_deref() {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(value) {
                        collect_references(&json, &mut roots, &mut _external);
                    }
                }
            }
        }
        let mut queue: std::collections::VecDeque<ObjectRef> = roots.into();
        queue.extend(manifest.objects.iter().map(|descriptor| ObjectRef {
            digest: descriptor.digest.clone(),
            bytes: descriptor.bytes,
            media_type: descriptor.media_type.clone(),
            store_namespace: descriptor.store_namespace.clone(),
            locator: String::new(),
            codec: descriptor.codec.clone(),
        }));
        let mut verified: BTreeMap<String, u64> = BTreeMap::new();
        let mut expanded: BTreeSet<String> = BTreeSet::new();
        while let Some(object) = queue.pop_front() {
            let key = object.digest.to_string();
            if !listed.contains(&key) {
                gaps.push(CognitionGap {
                    kind: "missing_object".into(),
                    detail: key,
                    blocking: true,
                });
                continue;
            }
            let expand = is_json_reference(&object.media_type) && !expanded.contains(&key);
            if let Some(&size) = verified.get(&key) {
                if size != object.bytes {
                    gaps.push(CognitionGap {
                        kind: "corrupt_retained_object".into(),
                        detail: key,
                        blocking: true,
                    });
                    continue;
                }
                if !expand {
                    continue;
                }
            }
            let bytes = match self.cas.read_object(&object.digest) {
                Ok(bytes) => bytes,
                Err(error) => {
                    gaps.push(CognitionGap {
                        kind: "unreadable_retained_object".into(),
                        detail: format!("{}: {error}", object.digest),
                        blocking: true,
                    });
                    continue;
                }
            };
            if bytes.len() as u64 != object.bytes || Digest::blake3(&bytes) != object.digest
            {
                gaps.push(CognitionGap {
                    kind: "corrupt_retained_object".into(),
                    detail: key,
                    blocking: true,
                });
                continue;
            }
            verified.insert(key.clone(), object.bytes);
            if expand {
                expanded.insert(key);
                if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    let mut nested: Vec<ObjectRef> = Vec::new();
                    let mut external: Vec<Digest> = Vec::new();
                    collect_references(&json, &mut nested, &mut external);
                    queue.extend(nested);
                }
            }
        }
        gaps
    }
}

/// Opaque bytes never acquire reference-graph authority by parsing as JSON.
/// Both export and import use this rule, including later typed aliases of a
/// digest already visited under an opaque reference.
fn is_json_reference(media_type: &str) -> bool {
    media_type == "application/json" || media_type.ends_with("+json")
}

/// Collect every CAS reference of a JSON value.
fn collect_references(
    value: &serde_json::Value,
    roots: &mut Vec<ObjectRef>,
    external: &mut Vec<Digest>,
) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(object) = object_ref_from(map) {
                roots.push(object);
                return;
            }
            if let Some(digest) = digest_from(map) {
                external.push(digest);
                // A map that names a digest but is not a whole object
                // reference is still a container: descend into it as well,
                // so a manifest that names its artifact inline does not
                // hide the objects it points at. The named digest itself is
                // skipped — it was already recorded as the alias.
                for (name, nested) in map {
                    if name == "digest" {
                        continue;
                    }
                    collect_references(nested, roots, external);
                }
                return;
            }
            for nested in map.values() {
                collect_references(nested, roots, external);
            }
        }
        serde_json::Value::Array(items) => {
            for nested in items {
                collect_references(nested, roots, external);
            }
        }
        serde_json::Value::String(text) => {
            if let Some(digest) = parse_digest(text) {
                external.push(digest);
            }
        }
        _ => {}
    }
}

fn object_ref_from(map: &serde_json::Map<String, serde_json::Value>) -> Option<ObjectRef> {
    let digest = map.get("digest").and_then(serde_json::Value::as_str)?;
    let digest = parse_digest(digest)?;
    let bytes = map.get("bytes").and_then(serde_json::Value::as_u64)?;
    let media_type = map.get("media_type").and_then(serde_json::Value::as_str)?;
    let store_namespace = map
        .get("store_namespace")
        .and_then(serde_json::Value::as_str)?;
    let codec: SchemaRef = serde_json::from_value(map.get("codec")?.clone()).ok()?;
    Some(ObjectRef {
        digest,
        bytes,
        media_type: media_type.to_string(),
        store_namespace: store_namespace.to_string(),
        locator: String::new(),
        codec,
    })
}

fn digest_from(map: &serde_json::Map<String, serde_json::Value>) -> Option<Digest> {
    let digest = map.get("digest").and_then(serde_json::Value::as_str)?;
    parse_digest(digest)
}

fn parse_digest(text: &str) -> Option<Digest> {
    let hex = text.strip_prefix("blake3:")?;
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(Digest(text.to_string()))
}

pub(super) fn raw_codec() -> SchemaRef {
    SchemaRef {
        name: "raw".into(),
        major: 1,
        digest: Digest::blake3(b"raw"),
    }
}
