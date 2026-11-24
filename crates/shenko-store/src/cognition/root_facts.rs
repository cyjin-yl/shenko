//! Required cognitive-root and refused-evidence facts, without re-keying them.
//!
//! The root writer allocates revisions from one global MAX, not per agent or
//! ledger. Its complete incoming chain and content/attitude parents therefore
//! form one append-only conflict unit. A collision cannot splice a peer tail
//! onto a different local chain, even when the peer baseline still matches.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, anyhow, ensure};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::de::DeserializeOwned;
use shenko_types::cognition::{AttitudeConfidence, AttitudeRole, ContentRecord, ReasonPointer};
use shenko_types::cognitive_root::{
    CognitiveRootNode, CognitiveRootRequest, EffectDisposition, RootApplicationScope,
    RootApplicationState, RootRef, RootRequestKind, COGNITIVE_ROOT_CODEC,
    COGNITIVE_ROOT_CODEC_MAJOR, COGNITIVE_ROOT_NAMESPACE,
};
use shenko_types::commit::CommitRef;
use shenko_types::evidence::EvidenceReceipt;
use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::object::{Digest, ObjectRef};
use shenko_types::operation::AttemptRecord;

use crate::cognition_archive::{CognitionManifest, CognitionRow};
use crate::receipt_admission::{ConflictIdentity, EvidenceReceiptConflict};

use super::peers::{record_conflict, record_origin};
use super::rows::{TABLES, insert_row, read_existing, rows_equivalent, spec_for, write_baseline};
use super::{CognitionApplyOutcome, CognitionConflict, db, is_constraint_violation};

fn field<'a>(row: &'a CognitionRow, column: &str) -> Result<&'a str> {
    row.value(column).ok_or_else(|| anyhow!("{} {} lacks {column}", row.table, row.key))
}

fn json<T: DeserializeOwned>(row: &CognitionRow, column: &str) -> Result<T> {
    serde_json::from_str(field(row, column)?)
        .with_context(|| format!("invalid {} {} {column}", row.table, row.key))
}

fn optional_json<T: DeserializeOwned>(row: &CognitionRow, column: &str) -> Result<Option<T>> {
    row.value(column).map(serde_json::from_str).transpose()
        .with_context(|| format!("invalid {} {} {column}", row.table, row.key))
}

fn integer(row: &CognitionRow, column: &str) -> Result<i64> {
    let text = field(row, column)?;
    let value: i64 = text.parse().with_context(|| format!("invalid {} {column}", row.table))?;
    ensure!(value.to_string() == text, "noncanonical {} {column}", row.table);
    Ok(value)
}

fn positive(row: &CognitionRow, column: &str) -> Result<u64> {
    let value = integer(row, column)?;
    ensure!(value > 0, "{} {column} must be positive", row.table);
    Ok(value as u64)
}

fn id(row: &CognitionRow, column: &str) -> Result<EntityId> {
    let text = field(row, column)?;
    let value: EntityId = text.parse().with_context(|| format!("invalid {} {column}", row.table))?;
    ensure!(value.to_string() == text, "noncanonical {} {column}", row.table);
    Ok(value)
}

fn commit(row: &CognitionRow, ledger: &str, seq: &str) -> Result<CommitRef> {
    Ok(CommitRef::new(id(row, ledger)?, LedgerSeq(positive(row, seq)?)))
}

fn validate_ref(reference: &RootRef) -> Result<()> {
    ensure!(reference.root_revision > 0 && reference.root_revision <= i64::MAX as u64
        && reference.content.canonical_blake3_hex().is_some(), "invalid cognitive root reference");
    Ok(())
}

fn is_root_fact(table: &str) -> bool {
    matches!(table, "cognitive_root_nodes" | "cognitive_root_requests"
        | "cognitive_root_effects" | "cognitive_root_applications")
}

fn node(row: &CognitionRow) -> Result<CognitiveRootNode> {
    // ContentRecord itself predates strict shape admission. Do not let a
    // malformed record fall through to the generic JSON closure scanner.
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StoredContent {
        object: ObjectRef,
        registered_at_ms: i64,
    }
    let stored: StoredContent = json(row, "content_record_json")?;
    let content = ContentRecord { object: stored.object, registered_at_ms: stored.registered_at_ms };
    let object = &content.object;
    ensure!(field(row, "content_digest")? == object.digest.0
        && object.digest.canonical_blake3_hex().is_some()
        && object.bytes <= i64::MAX as u64
        && object.media_type == "application/json"
        && object.store_namespace == COGNITIVE_ROOT_NAMESPACE
        && object.codec.name == COGNITIVE_ROOT_CODEC
        && object.codec.major == COGNITIVE_ROOT_CODEC_MAJOR
        && object.codec.digest == Digest::blake3(COGNITIVE_ROOT_CODEC.as_bytes()),
        "cognitive root ContentRecord digest or required codec differs");
    ensure!(integer(row, "schema_version")? == i64::from(CognitiveRootNode::SCHEMA_VERSION),
        "unsupported cognitive root node schema");
    let reinstated_from: Option<RootRef> = optional_json(row, "reinstated_from_json")?;
    if let Some(target) = &reinstated_from { validate_ref(target)?; }
    Ok(CognitiveRootNode {
        schema_version: CognitiveRootNode::SCHEMA_VERSION,
        agent_id: id(row, "agent_id")?, root_revision: positive(row, "root_revision")?,
        content, attitude_id: id(row, "attitude_id")?, fence: json(row, "fence_json")?,
        reinstated_from, recorded_commit: commit(row, "recorded_commit_ledger", "recorded_commit_seq")?,
        recorded_at_ms: integer(row, "recorded_at_ms")?,
    })
}

/// Typed admission runs on exports and at both import seams, before any file,
/// Git, SQL fact or baseline is published. References resolve in the archive,
/// never against a coincidentally matching local revision or preseeded CAS.
pub(super) fn validate_root_facts(rows: &[CognitionRow]) -> Result<()> {
    validate_evidence_conflicts(rows)?;
    if !rows.iter().any(|row| is_root_fact(&row.table)) { return Ok(()); }
    let mut identities = HashSet::new();
    for row in rows.iter().filter(|row| is_root_fact(&row.table)) {
        ensure!(identities.insert((row.table.as_str(), row.key.as_str())),
            "duplicate cognitive root fact identity; storage autoids cannot distinguish public facts");
    }
    let contents: HashMap<&str, &CognitionRow> = rows.iter().filter(|row| row.table == "cognition_contents")
        .map(|row| Ok((field(row, "digest")?, row))).collect::<Result<_>>()?;
    let attitudes: HashMap<&str, &CognitionRow> = rows.iter().filter(|row| row.table == "cognition_attitudes")
        .map(|row| Ok((field(row, "attitude_id")?, row))).collect::<Result<_>>()?;
    let mut nodes = BTreeMap::new();
    let mut node_attitudes = HashSet::new();
    for row in rows.iter().filter(|row| row.table == "cognitive_root_nodes") {
        let node = node(row)?;
        ensure!(node_attitudes.insert(node.attitude_id), "cognitive root nodes share an attitude identity");
        let parent = contents.get(node.content.digest().0.as_str()).context("cognitive root content parent missing")?;
        let object: ObjectRef = json(parent, "object_json")?;
        ensure!(node.content.object == object
            && node.content.registered_at_ms == integer(parent, "registered_at_ms")?
            && integer(parent, "bytes")? == object.bytes as i64
            && parent.value("media_type") == Some(object.media_type.as_str())
            && parent.value("store_namespace") == Some(object.store_namespace.as_str())
            && parent.value("locator") == Some(object.locator.as_str())
            && parent.value("codec_name") == Some(object.codec.name.as_str())
            && integer(parent, "codec_major")? == i64::from(object.codec.major)
            && parent.value("codec_digest") == Some(object.codec.digest.0.as_str()),
            "cognitive root ContentRecord disagrees with its registered content parent");
        let attitude_id = node.attitude_id.to_string();
        let attitude = attitudes.get(attitude_id.as_str()).context("cognitive root attitude parent missing")?;
        let role: AttitudeRole = json(attitude, "role_json")?;
        let _: Vec<ReasonPointer> = json(attitude, "reasons_json")?;
        let _: Option<AttitudeConfidence> = optional_json::<Option<AttitudeConfidence>>(attitude, "confidence_json")?.flatten();
        ensure!(id(attitude, "agent_id")? == node.agent_id
            && attitude.value("digest") == Some(node.content.digest().0.as_str())
            && role == AttitudeRole::Assertion && attitude.value("role_kind") == Some(role.kind())
            && commit(attitude, "created_commit_ledger", "created_commit_seq")? == node.recorded_commit,
            "cognitive root attitude parent binding differs");
        ensure!(nodes.insert(node.root_revision, node).is_none(), "duplicate cognitive root revision");
    }
    // The native writer allocates MAX(root_revision)+1 and never deletes.
    // Missing captures are missing history even without a reinstatement edge.
    for (index, revision) in nodes.keys().enumerate() {
        ensure!(*revision == index as u64 + 1, "cognitive root chain omits an earlier revision");
    }
    for node in nodes.values() {
        if let Some(target) = &node.reinstated_from {
            let parent = nodes.get(&target.root_revision).context("cognitive root reinstatement target missing")?;
            ensure!(target.root_revision < node.root_revision && target.matches(parent)
                && node.content == parent.content, "cognitive root reinstatement target binding differs");
        }
    }
    let mut requested = HashSet::new();
    for row in rows.iter().filter(|row| row.table == "cognitive_root_requests") {
        let request = CognitiveRootRequest {
            request_id: id(row, "request_id")?, agent_id: id(row, "agent_id")?,
            kind: match field(row, "kind")? {
                "capture" => RootRequestKind::Capture, "reinstate" => RootRequestKind::Reinstate,
                _ => anyhow::bail!("invalid cognitive root request kind"),
            },
            root_revision: positive(row, "root_revision")?, content: Digest(field(row, "content_digest")?.into()),
            target: optional_json(row, "target_json")?,
            recorded_commit: commit(row, "recorded_commit_ledger", "recorded_commit_seq")?,
            recorded_at_ms: integer(row, "recorded_at_ms")?,
        };
        let node = nodes.get(&request.root_revision).context("cognitive root request node parent missing")?;
        let target_kind = if request.target.is_some() { RootRequestKind::Reinstate } else { RootRequestKind::Capture };
        ensure!(request.agent_id == node.agent_id && &request.content == node.content.digest()
            && request.recorded_commit == node.recorded_commit && request.recorded_at_ms == node.recorded_at_ms
            && request.target == node.reinstated_from && request.kind == target_kind,
            "cognitive root request kind/target/node binding differs");
        ensure!(requested.insert(request.root_revision), "cognitive root node has multiple request identities");
    }
    ensure!(requested.len() == nodes.len(), "cognitive root node request parent missing");
    let mut application_sequences = HashMap::new();
    for row in rows.iter().filter(|row| matches!(row.table.as_str(), "cognitive_root_effects" | "cognitive_root_applications")) {
        let revision = positive(row, "root_revision")?;
        ensure!(nodes.contains_key(&revision), "cognitive root child node parent missing");
        if row.table == "cognitive_root_effects" {
            id(row, "effect_id")?;
            let _: EffectDisposition = match field(row, "disposition")? {
                "unknown" => EffectDisposition::Unknown, "reconciled" => EffectDisposition::Reconciled,
                _ => anyhow::bail!("invalid cognitive root effect disposition"),
            };
        } else {
            let _: RootApplicationScope = match field(row, "scope")? {
                "home_files" => RootApplicationScope::HomeFiles,
                _ => anyhow::bail!("invalid cognitive root application scope"),
            };
            let _: RootApplicationState = json(row, "state_json")?;
            let commit = commit(row, "updated_commit_ledger", "updated_commit_seq")?;
            ensure!(application_sequences.insert((revision, commit.ledger_id), commit.seq.0)
                .is_none_or(|previous| previous < commit.seq.0),
                "cognitive root application history reverses its original ledger order");
        }
    }
    Ok(())
}

fn validate_evidence_conflicts(rows: &[CognitionRow]) -> Result<()> {
    let mut conflicts = rows.iter().filter(|row| row.table == "evidence_receipt_conflicts").peekable();
    if conflicts.peek().is_none() { return Ok(()); }
    let receipts: HashMap<&str, &CognitionRow> = rows.iter().filter(|row| row.table == "evidence_receipts")
        .map(|row| Ok((field(row, "receipt_id")?, row))).collect::<Result<_>>()?;
    let attempts: HashMap<&str, &CognitionRow> = rows.iter().filter(|row| row.table == "attempts")
        .map(|row| Ok((field(row, "attempt_id")?, row))).collect::<Result<_>>()?;
    for row in conflicts {
        let conflict: EvidenceReceiptConflict = json(row, "data")?;
        let receipt = &conflict.receipt;
        let operation = row.value("operation_id").map(str::parse::<EntityId>).transpose()?;
        ensure!(conflict.schema_version == 1 && id(row, "conflict_id")? == conflict.conflict_id
            && id(row, "receipt_id")? == conflict.receipt_id && conflict.receipt_id == receipt.receipt_id
            && id(row, "original_receipt_id")? == conflict.original_receipt_id
            && operation == conflict.operation_id && operation == receipt.operation_id
            && conflict.attempt_id == receipt.attempt_id
            && row.value("source_id") == Some(conflict.source_id.as_str()) && conflict.source_id == receipt.source_id
            && row.value("source_instance_id") == Some(conflict.source_instance_id.as_str())
            && conflict.source_instance_id == receipt.source_instance_id
            && row.value("source_event_key") == Some(conflict.source_event_key.as_str())
            && conflict.source_event_key == receipt.source_event_key.as_deref().unwrap_or_default()
            && positive(row, "updated_commit_seq")? == receipt.received_commit.seq.0
            && !conflict.differing_fields.is_empty() && receipt.raw_ref.digest.canonical_blake3_hex().is_some(),
            "evidence receipt conflict original/provenance binding differs");
        match conflict.matched {
            ConflictIdentity::ReceiptId => ensure!(conflict.original_receipt_id == conflict.receipt_id,
                "evidence receipt identity conflict names a different original"),
            ConflictIdentity::SourceEventKey => ensure!(!conflict.source_event_key.is_empty()
                && conflict.original_receipt_id != conflict.receipt_id,
                "evidence source-event conflict lacks its original scoped identity"),
        }
        if let Some(original) = receipts.get(conflict.original_receipt_id.to_string().as_str()) {
            let original_receipt: EvidenceReceipt = json(original, "data")?;
            ensure!(original_receipt.receipt_id == conflict.original_receipt_id
                && original.value("operation_id").map(str::parse::<EntityId>).transpose()? == original_receipt.operation_id,
                "evidence receipt conflict original receipt binding differs");
            if conflict.matched == ConflictIdentity::SourceEventKey {
                ensure!(original_receipt.source_id == conflict.source_id
                    && original_receipt.source_instance_id == conflict.source_instance_id
                    && original_receipt.source_event_key.as_deref() == Some(conflict.source_event_key.as_str()),
                    "evidence receipt conflict original source-event scope differs");
            }
        }
        // X04 permits foreign/unknown operation, attempt and original-receipt
        // identities. Their absence is not an orphan. When the archive does
        // carry an attempt, its actual operation binding must still agree.
        if let Some(attempt) = conflict.attempt_id.and_then(|attempt| attempts.get(attempt.to_string().as_str()))
        {
            let record: AttemptRecord = json(attempt, "data")?;
            ensure!(Some(record.attempt_id) == conflict.attempt_id && Some(record.operation_id) == operation
                && attempt.value("operation_id").map(str::parse::<EntityId>).transpose()? == operation,
                "evidence receipt conflict attempt/operation binding differs");
        }
    }
    Ok(())
}

pub(super) fn apply_root_chain<'a>(
    tx: &Transaction<'_>, rows: &[&'a CognitionRow], peer_id: EntityId, seq: LedgerSeq,
    manifest: &CognitionManifest,
) -> Result<(CognitionApplyOutcome, HashSet<(&'a str, &'a str)>)> {
    let digests: HashSet<&str> = rows.iter().filter(|row| row.table == "cognitive_root_nodes")
        .filter_map(|row| row.value("content_digest")).collect();
    let mut group: Vec<&CognitionRow> = rows.iter().copied().filter(|row| {
        is_root_fact(&row.table)
            || (matches!(row.table.as_str(), "cognition_contents" | "cognition_attitudes")
                && row.value("digest").is_some_and(|digest| digests.contains(digest)))
            || (row.table == "cognition_legacy_content"
                && row.value("content_digest").is_some_and(|digest| digests.contains(digest)))
    }).collect();
    let owned = group.iter().map(|row| (row.table.as_str(), row.key.as_str())).collect();
    let mut outcome = CognitionApplyOutcome::default();
    if group.is_empty() { return Ok((outcome, owned)); }
    // Stable sorting retains the producer's append order of applications.
    group.sort_by_key(|row| TABLES.iter().position(|spec| spec.name == row.table));
    tx.execute("SAVEPOINT cognitive_root_chain", []).map_err(db)?;
    let mut cause = None;
    let mut application_positions = HashMap::new();
    let mut new_applications = HashSet::new();
    for row in &group {
        let spec = spec_for(&row.table).context("unregistered cognitive root fact")?;
        if row.table == "cognitive_root_requests" {
            let original: Option<String> = tx.query_row(
                "SELECT request_id FROM cognitive_root_requests WHERE root_revision=?1 AND request_id<>?2 LIMIT 1",
                params![row.value("root_revision"), row.value("request_id")], |row| row.get(0),
            ).optional().map_err(db)?;
            if let Some(original) = original {
                cause = Some(format!("root {} already belongs to request {original}; request identities cannot be replaced",
                    field(row, "root_revision")?));
                break;
            }
        }
        if row.table == "cognitive_root_applications" {
            let revision = field(row, "root_revision")?;
            let position: Option<i64> = tx.query_row(
                "SELECT id FROM cognitive_root_applications WHERE root_revision=?1 AND scope=?2
                 AND updated_commit_ledger=?3 AND updated_commit_seq=?4",
                params![revision, row.value("scope"), row.value("updated_commit_ledger"), row.value("updated_commit_seq")],
                |row| row.get(0),
            ).optional().map_err(db)?;
            if let Some(position) = position {
                if new_applications.contains(revision)
                    || application_positions.insert(revision, position).is_some_and(|previous| previous >= position)
                {
                    cause = Some(format!("root {revision} application history changes its original append order"));
                    break;
                }
            } else {
                new_applications.insert(revision);
            }
        }
        match read_existing(tx, spec, row)? {
            Some(local) if rows_equivalent(spec, &local, &row.values) => outcome.unchanged += 1,
            Some(_) => {
                cause = Some(format!("{} {} differs from its immutable original", row.table, row.key));
                break;
            }
            None => match insert_row(tx, spec, row, seq) {
                Ok(()) => {
                    record_origin(tx, spec, row, peer_id, &manifest.fingerprint, manifest.source_ledger_id)?;
                    outcome.inserted += 1;
                }
                Err(error) if is_constraint_violation(&error) => {
                    cause = Some(format!("{} {} insert constraint: {error:#}", row.table, row.key));
                    break;
                }
                Err(error) => return Err(error),
            },
        }
        write_baseline(tx, peer_id, spec.name, &row.key, &row.values)?;
    }
    if let Some(cause) = cause {
        tx.execute("ROLLBACK TO cognitive_root_chain", []).map_err(db)?;
        outcome = CognitionApplyOutcome::default();
        for row in group {
            let detail = format!("global cognitive root chain rejected: {cause}; all incoming nodes, parents, requests, effects, applications and baselines are kept out");
            record_conflict(tx, peer_id, &row.table, &row.key, &detail)?;
            outcome.conflicts.push(CognitionConflict { table: row.table.clone(), key: row.key.clone(), detail });
        }
    }
    tx.execute("RELEASE cognitive_root_chain", []).map_err(db)?;
    Ok((outcome, owned))
}

#[cfg(test)]
#[path = "root_facts_tests.rs"]
mod tests;
