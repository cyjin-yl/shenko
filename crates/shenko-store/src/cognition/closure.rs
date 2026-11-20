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

    /// Discover roots directly from bounded borrowed SQL cells, retaining the
    /// same schema authority as the owned archive rows.
    pub(crate) fn cognition_text_object_roots<'a>(
        &self,
        values: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
    ) -> (Vec<ObjectRef>, Vec<Digest>) {
        let (mut roots, external) = declared_text_object_roots(values);
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

    /// Apply finite transport budgets to the same schema-aware closure.
    pub(crate) fn collect_closure_bounded(
        &self,
        roots: &[ObjectRef],
        max_objects: usize,
        max_bytes: u64,
    ) -> Result<(Vec<CognitionObjectDescriptor>, Vec<Digest>)> {
        // One schema-aware traversal verifies required bare-digest children
        // as well as complete ObjectRefs. Retain the strongest declaration
        // when opaque, generic JSON and required-child aliases share bytes.
        let mut objects: BTreeMap<Digest, ((bool, bool), CognitionObjectDescriptor)> = BTreeMap::new();
        let closure = crate::roots::object_closure_with_limits(&self.cas, roots.to_vec(), max_objects, max_bytes, |object, required| {
            let priority = (required, is_json_reference(&object.media_type));
            let descriptor = || CognitionObjectDescriptor {
                digest: object.digest.clone(),
                bytes: object.bytes,
                media_type: object.media_type.clone(),
                store_namespace: object.store_namespace.clone(),
                codec: object.codec.clone(),
            };
            match objects.entry(object.digest.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert((priority, descriptor()));
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    if priority > entry.get().0 {
                        entry.insert((priority, descriptor()));
                    }
                }
            }
        })?;
        Ok((objects.into_values().map(|(_, object)| object).collect(),
            closure.missing.into_iter().collect()))
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
        let listed: BTreeSet<Digest> = manifest
            .objects
            .iter()
            .map(|descriptor| descriptor.digest.clone())
            .collect();
        // Optional graph/checksum identities are not required membership.
        // Required bare children are discovered by their declared schema.
        let (mut roots, _) = declared_object_roots(&manifest.rows);
        roots.reserve(manifest.objects.len());
        roots.extend(manifest.objects.iter().map(|descriptor| ObjectRef {
            digest: descriptor.digest.clone(),
            bytes: descriptor.bytes,
            media_type: descriptor.media_type.clone(),
            store_namespace: descriptor.store_namespace.clone(),
            locator: String::new(),
            codec: descriptor.codec.clone(),
        }));
        let closure = crate::roots::object_closure(&self.cas, roots);
        let mut missing = closure.missing;
        missing.extend(closure.digests.into_iter().filter(|digest| !listed.contains(digest)));
        missing.into_iter().map(|digest| CognitionGap {
            kind: if listed.contains(&digest) { "corrupt_retained_object" } else { "missing_object" }.into(),
            detail: digest.to_string(),
            blocking: true,
        }).collect()
    }
}

/// Row schema supplies authority independently of an earlier opaque alias.
fn declared_object_roots(rows: &[CognitionRow]) -> (Vec<ObjectRef>, Vec<Digest>) {
    declared_text_object_roots(rows.iter().flat_map(|row| {
        row.values.iter().filter_map(move |(name, value)| {
            value.as_deref().map(|value| (row.table.as_str(), name.as_str(), value))
        })
    }))
}

pub(crate) fn declared_text_object_roots<'a>(
    values: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
) -> (Vec<ObjectRef>, Vec<Digest>) {
    let mut roots = Vec::new();
    let mut external = Vec::new();
    for (table, name, value) in values {
        if table == "cognitive_root_nodes" && name == "content_record_json" {
            if let Ok(content) = serde_json::from_str::<shenko_types::cognition::ContentRecord>(value) {
                roots.push(crate::roots::cognitive_root_reference(content.object));
                continue;
            }
        }
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(value) {
            collect_references(&json, &mut roots, &mut external);
        } else if table == "context_notes" && name == "body" {
            // Authored free text stays optional and byte-preserving.
            if let Some(digest) = parse_digest(value) { external.push(digest); }
        }
    }
    (roots, external)
}

/// Opaque bytes never acquire reference-graph authority by parsing as JSON.
/// Both export and import use this rule, including later typed aliases of a
/// digest already visited under an opaque reference.
pub(crate) fn is_json_reference(media_type: &str) -> bool {
    media_type == "application/json" || media_type.ends_with("+json")
}

/// Collect complete typed references without treating identifiers as objects.
pub(crate) fn collect_object_references(value: &serde_json::Value, roots: &mut Vec<ObjectRef>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(object) = object_ref_from(map) {
                roots.push(object);
            } else {
                for nested in map.values() {
                    collect_object_references(nested, roots);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for nested in items {
                collect_object_references(nested, roots);
            }
        }
        _ => {}
    }
}

/// Collect every CAS reference of a JSON value.
pub(crate) fn collect_references(
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
