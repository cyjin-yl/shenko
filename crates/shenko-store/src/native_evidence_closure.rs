//! Versioned E1 increment over independently verified local predecessors.
//!
//! The original inner facts and full graph edges remain immutable. The new
//! envelope does not turn an imported CAS object into a local application.
use std::collections::BTreeMap;

use rusqlite::Connection;
use shenko_types::{
    authority::CommitAdmissionObject,
    error::ShenkoError,
    native_evidence::{
        AnchoredNativeEvidence, NativeEvidenceObjectIdentity,
        NATIVE_EVIDENCE_ANCHORED_CODEC_MAJOR, NATIVE_EVIDENCE_CLOSURE_BYTES,
        NATIVE_EVIDENCE_CLOSURE_OBJECTS,
    },
    object::{Digest, ObjectRef},
};

use crate::{
    Store,
    native_evidence::{CODEC, NativeEvidenceFact, read_materialization, verify_materialization},
    native_facts::{CanonicalFact, conflict, invalid, same, typed_json},
    store::storage_error,
};

pub(crate) fn predecessor_roots(fact: &NativeEvidenceFact) -> Vec<ObjectRef> {
    let mut roots = vec![fact.arm.admission.payload_ref.clone()];
    roots.extend(fact.source.iter().map(|source| source.evidence_ref.clone()));
    roots.extend(fact.predecessor_ref.iter().cloned());
    roots.sort_by_key(NativeEvidenceObjectIdentity::from);
    roots.dedup_by(|left, right|
        NativeEvidenceObjectIdentity::from(&*left) == NativeEvidenceObjectIdentity::from(&*right));
    roots
}

fn decode_fact(
    reference: &ObjectRef,
    mut load: impl FnMut(&ObjectRef) -> Result<Vec<u8>, ShenkoError>,
) -> Result<(NativeEvidenceFact, Option<AnchoredNativeEvidence>), ShenkoError> {
    let envelope = if reference.codec.major == NATIVE_EVIDENCE_ANCHORED_CODEC_MAJOR {
        typed_json(reference, CODEC, NATIVE_EVIDENCE_ANCHORED_CODEC_MAJOR)?;
        Some(AnchoredNativeEvidence::decode(reference, &load(reference)?).map_err(invalid)?)
    } else { None };
    let original = envelope.as_ref().map_or(reference, |envelope| &envelope.fact_ref);
    if !matches!(original.codec.major, 1..=5) {
        return Err(invalid("unsupported typed object for original E1"));
    }
    typed_json(original, CODEC, original.codec.major)?;
    let bytes = load(original)?;
    if bytes.len() as u64 != original.bytes || Digest::blake3(&bytes) != original.digest {
        return Err(invalid("original E1 fact bytes differ"));
    }
    let fact: NativeEvidenceFact = serde_json::from_slice(&bytes).map_err(invalid)?;
    if fact.schema_version != original.codec.major {
        return Err(invalid("E1 codec and original fact schema disagree"));
    }
    if envelope.is_none() && fact.source.iter().map(|source| &source.evidence_ref)
        .chain(fact.predecessor_ref.iter()).any(|reference| reference.codec.major >= NATIVE_EVIDENCE_ANCHORED_CODEC_MAJOR)
    {
        return Err(invalid("original E1 codec does not accept an anchored predecessor"));
    }
    if let Some(envelope) = &envelope {
        let expected = predecessor_roots(&fact);
        if !expected.iter().map(NativeEvidenceObjectIdentity::from)
            .eq(envelope.predecessor_roots.iter().map(NativeEvidenceObjectIdentity::from))
        {
            return Err(invalid("anchored E1 predecessor list differs from its original fact"));
        }
    }
    Ok((fact, envelope))
}

impl Store {
    /// Only decode here. Callers still validate the original fold and each
    /// actual application before treating the predecessor graph as a boundary.
    pub(crate) fn read_native_evidence_object(
        &self,
        reference: &ObjectRef,
    ) -> Result<(NativeEvidenceFact, Option<AnchoredNativeEvidence>), ShenkoError> {
        decode_fact(reference, |reference| self.cas.read_object(&reference.digest))
    }

    /// Inspect incoming root/fact bytes without publishing anything into CAS.
    /// In particular, an E1 transfer may not repair its own missing base.
    pub(crate) fn incoming_native_evidence_object(
        &self,
        reference: &ObjectRef,
        objects: &[CommitAdmissionObject],
    ) -> Result<(NativeEvidenceFact, Option<AnchoredNativeEvidence>), ShenkoError> {
        decode_fact(reference, |reference| {
            let identity = NativeEvidenceObjectIdentity::from(reference);
            match objects.iter().find(|object| NativeEvidenceObjectIdentity::from(&object.reference) == identity) {
                Some(object) => Ok(object.bytes.clone()),
                None => self.cas.read_object(&reference.digest),
            }
        })
    }

    /// The actual T1 receipt is checked, not inferred from a closed CAS graph.
    /// Source/Unknown markers likewise require exact original receipts and
    /// local commits. Full fold/source validation remains with the reader.
    pub(crate) fn verify_anchored_native_predecessors(
        &self,
        conn: &Connection,
        fact: &NativeEvidenceFact,
    ) -> Result<(), ShenkoError> {
        self.verify_native_arm_fact(conn, &fact.arm)?;
        for reference in fact.source.iter().map(|source| &source.evidence_ref)
            .chain(fact.predecessor_ref.iter())
        {
            let original = self.applied_native_evidence_predecessor(conn, reference)?;
            if let Some(source) = &fact.source
                && source.evidence_ref == *reference && !same(&source.operation, &original.operation)?
            {
                return Err(invalid("anchored E1 source snapshot differs from its actual application"));
            }
        }
        Ok(())
    }

    fn applied_native_evidence_predecessor(
        &self,
        conn: &Connection,
        reference: &ObjectRef,
    ) -> Result<NativeEvidenceFact, ShenkoError> {
        // Check custody before reading the graph: a missing source/Unknown
        // application is durable predecessor debt, not a request to fetch it.
        let mut statement = conn.prepare("SELECT receipt_id FROM native_evidence_materializations WHERE json_extract(data,'$.evidence_ref.digest')=?1 LIMIT 2")
            .map_err(storage_error)?;
        let rows = statement.query_map([reference.digest.to_string()], |row| row.get::<_, String>(0))
            .map_err(storage_error)?.collect::<rusqlite::Result<Vec<_>>>().map_err(storage_error)?;
        let [id] = rows.as_slice() else {
            return Err(conflict("anchored E1 predecessor has no unique actual local application"));
        };
        let id = id.parse().map_err(invalid)?;
        let marker = read_materialization(conn, id)?
            .ok_or_else(|| conflict("anchored E1 predecessor application disappeared"))?;
        if NativeEvidenceObjectIdentity::from(&marker.evidence_ref)
            != NativeEvidenceObjectIdentity::from(reference)
        {
            return Err(conflict("anchored E1 predecessor application names different original bytes"));
        }
        let (fact, _) = self.read_native_evidence_object(reference)?;
        if fact.receipt.receipt_id != id {
            return Err(invalid("anchored E1 predecessor receipt identity differs"));
        }
        verify_materialization(conn, &marker, &fact)?;
        Ok(fact)
    }

    fn verify_native_evidence_predecessor_application(
        &self,
        conn: &Connection,
        root: &ObjectRef,
    ) -> Result<(), ShenkoError> {
        if root.codec.name == "commit-admission" && root.codec.major == 1 {
            typed_json(root, "commit-admission", 1)?;
            let mut statement = conn.prepare("SELECT data FROM control_materializations WHERE json_extract(data,'$.admission.payload_ref.digest')=?1 LIMIT 2")
                .map_err(storage_error)?;
            let rows = statement.query_map([root.digest.to_string()], |row| row.get::<_, String>(0))
                .map_err(storage_error)?.collect::<rusqlite::Result<Vec<_>>>().map_err(storage_error)?;
            let [raw] = rows.as_slice() else {
                return Err(conflict("native E1 base is not one actual local T1 application"));
            };
            let row: serde_json::Value = serde_json::from_str(raw).map_err(invalid)?;
            let fact = CanonicalFact {
                admission: serde_json::from_value(row["admission"].clone()).map_err(invalid)?,
                transition_ref: serde_json::from_value(row["transition_ref"].clone()).map_err(invalid)?,
            };
            if NativeEvidenceObjectIdentity::from(&fact.admission.payload_ref)
                != NativeEvidenceObjectIdentity::from(root)
            {
                return Err(invalid("native E1 T1 base has a different typed identity"));
            }
            self.verify_native_arm_fact(conn, &fact)?;
        } else if root.codec.name == CODEC && matches!(root.codec.major, 1..=6) {
            self.applied_native_evidence_predecessor(conn, root)?;
            self.read_native_delivery_fact_at(conn, root, Some(false))?;
        } else {
            return Err(invalid("unsupported native E1 predecessor type"));
        }
        Ok(())
    }

    /// Even an already closed Node cache must not substitute for this Store's
    /// actual application. Verify full local content without exporting bodies.
    pub fn validate_native_evidence_predecessor(&self, root: &ObjectRef) -> Result<(), ShenkoError> {
        let conn = self.ledger.conn.lock();
        self.verify_native_evidence_predecessor_application(&conn, root)?;
        let closure = crate::roots::object_closure(&self.cas, vec![root.clone()]);
        if !closure.missing.is_empty() {
            return Err(invalid("native E1 local predecessor closure is missing or corrupt"));
        }
        Ok(())
    }

    /// Seed the separate local Node CAS only from this Store's already applied
    /// predecessor. This is a local cache fill, never an E1 remote download or
    /// a replacement for Node's complete closure verification.
    pub fn native_evidence_predecessor_objects(
        &self,
        root: &ObjectRef,
    ) -> Result<Vec<CommitAdmissionObject>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        self.verify_native_evidence_predecessor_application(&conn, root)?;
        let (objects, missing) = self.collect_closure(std::slice::from_ref(root)).map_err(invalid)?;
        if !missing.is_empty() {
            return Err(invalid("native E1 local predecessor closure is missing or corrupt"));
        }
        objects.into_iter().map(|object| {
            let reference = ObjectRef {
                digest: object.digest, bytes: object.bytes, media_type: object.media_type,
                store_namespace: object.store_namespace, locator: String::new(), codec: object.codec,
            };
            let bytes = self.cas.read_object(&reference.digest)?;
            if bytes.len() as u64 != reference.bytes || Digest::blake3(&bytes) != reference.digest {
                return Err(invalid("native E1 local predecessor bytes changed"));
            }
            Ok(CommitAdmissionObject { reference, bytes })
        }).collect()
    }

    /// This unbounded predecessor verification streams opaque bodies. Only
    /// metadata is retained; a missing/corrupt base is never fixed by delta.
    pub(crate) fn verify_native_evidence_base(
        &self,
        envelope: &AnchoredNativeEvidence,
    ) -> Result<(), ShenkoError> {
        crate::roots::object_closure_increment_with_limits(
            &self.cas, Vec::new(), envelope.predecessor_roots.clone(),
            NATIVE_EVIDENCE_CLOSURE_OBJECTS, NATIVE_EVIDENCE_CLOSURE_BYTES, |_, _| {},
        ).map_err(invalid)?;
        Ok(())
    }

    /// All returned references were encountered in the increment itself.
    /// Preserve typed aliases here; export may select one of them per digest.
    pub(crate) fn native_evidence_closure(
        &self,
        reference: &ObjectRef,
        envelope: Option<&AnchoredNativeEvidence>,
    ) -> Result<Vec<ObjectRef>, ShenkoError> {
        if let Some(envelope) = envelope {
            let mut objects = BTreeMap::new();
            let closure = crate::roots::object_closure_increment_with_limits(
                &self.cas, vec![reference.clone()], envelope.predecessor_roots.clone(),
                NATIVE_EVIDENCE_CLOSURE_OBJECTS, NATIVE_EVIDENCE_CLOSURE_BYTES,
                |object, _| { objects.insert(NativeEvidenceObjectIdentity::from(object), object.clone()); },
            ).map_err(invalid)?;
            if !closure.missing.is_empty() {
                return Err(invalid("original E1 increment is incomplete"));
            }
            return Ok(objects.into_values().collect());
        }
        let (objects, missing) = self.collect_closure_bounded(std::slice::from_ref(reference),
            NATIVE_EVIDENCE_CLOSURE_OBJECTS, NATIVE_EVIDENCE_CLOSURE_BYTES).map_err(invalid)?;
        if !missing.is_empty() {
            return Err(invalid("original E1 closure is incomplete"));
        }
        Ok(objects.into_iter().map(|object| ObjectRef {
            digest: object.digest, bytes: object.bytes, media_type: object.media_type,
            store_namespace: object.store_namespace, locator: String::new(), codec: object.codec,
        }).collect())
    }
}
