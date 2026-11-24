//! Archive membership, legacy layouts and required-fact admission.
//!
//! These gates are shared by export, archive import and control-predecessor
//! validation. They retain the archive's existing schema and identity rules.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, anyhow, ensure};
use shenko_types::ids::EntityId;

use crate::cognition_archive::{CognitionGap, CognitionManifest, CognitionRow};
use crate::store::Store;

use super::rows::{TABLES, read_table, spec_for};
use super::snapshot::fingerprint_of_manifest;

impl Store {
    /// The control-plane predecessor uses the same membership, semantic-unit
    /// and CAS closure checks as a normal cognition import. It deliberately
    /// excludes installation credentials, writer leases and filesystem state.
    pub(crate) fn validate_control_manifest(&self, manifest: &CognitionManifest) -> Result<()> {
        ensure!(manifest.fingerprint == fingerprint_of_manifest(manifest), "control snapshot fingerprint differs");
        ensure!(manifest.files.is_empty() && manifest.file_origins.is_empty() && manifest.git_history.is_none(),
            "control predecessor is not a filesystem handover");
        check_archive_membership(manifest)?;
        ensure!(!LOCAL_CONTROL_CLASSES.iter().any(|class| manifest.fact_classes.iter().any(|declared| declared == *class)
            || manifest.rows.iter().any(|row| row.table == **class)), "control predecessor carries installation-local control");
        validate_cognition_rows(manifest)?;
        self.validate_obligation_rows(&manifest.rows)?;
        let mut gaps = manifest.gaps.clone();
        gaps.extend(memory_intent_binding_gaps(&manifest.rows));
        gaps.extend(crate::media::media_recovery_gaps(&manifest.rows));
        gaps.extend(archive_membership_gaps(&manifest.rows));
        gaps.extend(self.verify_manifest_closure(manifest));
        ensure!(!gaps.iter().any(|gap| gap.blocking), "control predecessor has an incomplete required fact closure: {gaps:?}");
        Ok(())
    }

    pub(crate) fn validate_obligation_recovery(&self, conn: &rusqlite::Connection) -> Result<()> {
        let mut rows = Vec::new();
        for spec in TABLES.iter().filter(|spec| matches!(spec.name,
            "subject_obligations" | "subject_steps" | "subject_branch_heads" | "subject_head_history" | "subject_intents" | "subject_intent_steps"
                | "channel_record_migrations" | "inbox_events"))
        {
            rows.extend(read_table(conn, spec)?);
        }
        self.validate_obligation_rows(&rows)
    }
}

/// Fact classes that are this installation's own control state rather than
/// shared cognition. A peer archive never carries them; only the declared
/// same-installation handover exports and installs them.
pub(super) const LOCAL_CONTROL_CLASSES: &[&str] = &["context_rollover_requests"];

// Membership became mandatory in schema 6. A later writer schema must not
// silently extend the legacy exemption to archives that already required it.
pub(super) const MEMBERSHIP_REQUIRED_SCHEMA: u32 = 6;

/// The fact classes whose members cannot exist without their anchor row in
/// the same archive: a member class, its anchor class, the member column
/// that carries the binding and the anchor column it must match.
///
/// These are the REQUIRED facts of the archive: a durable binding, a
/// tombstone or an armed effect. A peer that ships a member without its
/// anchor has not shipped a complete fact, and the receiver must refuse the
/// archive instead of admitting an orphan that no rule can ever complete.
const FACT_CLASS_ANCHORS: &[(&str, &str, &str, &str)] = &[
    ("content_stream_units", "content_streams", "stream_id", "stream_id"),
    ("seat_leases", "seat_records", "seat_id", "seat_id"),
    ("seat_questions", "seat_records", "seat_id", "seat_id"),
    ("seat_answer_decisions", "seat_questions", "question_id", "question_id"),
    ("research_candidates", "research_epochs", "epoch_id", "epoch_id"),
    ("intervention_bank_entries", "research_epochs", "epoch_id", "epoch_id"),
    ("intervention_reminders", "research_epochs", "epoch_id", "epoch_id"),
    ("memory_intent_bindings", "memory_intents", "intent_id", "intent_id"),
    ("quota_sends", "quota_attempts", "request_id", "request_id"),
    ("media_recordings", "media_streams", "stream_key", "stream_key"),
    ("media_recording_controls", "media_streams", "stream_key", "stream_key"),
    ("media_priority_inputs", "media_streams", "stream_key", "stream_key"),
    ("artifact_manifests", "artifact_records", "artifact_id", "artifact_id"),
    ("artifact_runtime_instances", "artifact_records", "artifact_id", "artifact_id"),
    // A cognitive attitude exists only about a registered version of content,
    // and a legacy mapping only about the bytes it points at. Shipping either
    // without its content registration would hand over an attitude that names
    // a revision the receiver can never read.
    ("cognition_attitudes", "cognition_contents", "digest", "digest"),
    ("cognition_legacy_content", "cognition_contents", "content_digest", "digest"),
    ("cognitive_root_nodes", "cognition_contents", "content_digest", "digest"),
    ("cognitive_root_nodes", "cognition_attitudes", "attitude_id", "attitude_id"),
    ("cognitive_root_requests", "cognitive_root_nodes", "root_revision", "root_revision"),
    ("cognitive_root_effects", "cognitive_root_nodes", "root_revision", "root_revision"),
    ("cognitive_root_applications", "cognitive_root_nodes", "root_revision", "root_revision"),
];

/// The fact a quota event describes: the payload field that names it, the
/// class that holds it and that class's id column.
const QUOTA_EVENT_CAUSES: &[(&str, &str, &str)] = &[
    ("snapshot_id", "quota_snapshots", "snapshot_id"),
    ("forecast_id", "quota_forecasts", "forecast_id"),
    ("decision_id", "quota_decisions", "decision_id"),
    ("policy_id", "quota_policies", "policy_id"),
];

pub(super) fn is_local_control_class(table: &str) -> bool {
    LOCAL_CONTROL_CLASSES.contains(&table)
}

/// The fact classes this reader replicates: the membership every archive
/// this build writes declares.
pub(super) fn local_fact_classes(include_local_control: bool) -> Vec<String> {
    let mut names: Vec<String> = TABLES
        .iter()
        .filter(|spec| include_local_control || !is_local_control_class(spec.name))
        .map(|spec| spec.name.to_string())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The archive's membership must be a membership this reader knows, and
/// every row must belong to it. An archive naming an unknown class, or
/// carrying a row outside its own declared membership, is refused whole:
/// the receiver never guesses which facts the archive meant to carry.
pub(super) fn check_archive_membership(manifest: &CognitionManifest) -> Result<()> {
    crate::cognition_archive::validate_manifest_schema(manifest)?;
    let known: HashSet<&str> = TABLES.iter().map(|spec| spec.name).collect();
    for class in &manifest.fact_classes {
        ensure!(
            known.contains(class.as_str()),
            "unsupported cognition fact class {}",
            class
        );
    }
    if manifest.fact_classes.is_empty() {
        // Only archives predating mandatory membership can omit it. This is
        // a format-introduction boundary, not the latest writer version.
        ensure!(
            manifest.schema < MEMBERSHIP_REQUIRED_SCHEMA,
            "cognition schema 6 and later must declare its fact classes"
        );
        return Ok(());
    }
    let declared: HashSet<&str> = manifest.fact_classes.iter().map(|name| name.as_str()).collect();
    for row in &manifest.rows {
        ensure!(
            declared.contains(row.table.as_str()),
            "cognition row {} is outside the archive's declared membership",
            row.table
        );
    }
    Ok(())
}

/// Migrate a legacy archive to the current column layout. The layout only
/// ever grew: a legacy row has no value for the columns this schema added,
/// so the migration is by NAME and every fact keeps its identity. A legacy
/// column this schema no longer knows is reported as a non-blocking gap
/// instead of disappearing silently.
pub(super) fn migrate_legacy_rows(manifest: &mut CognitionManifest) -> Vec<CognitionGap> {
    let mut gaps = Vec::new();
    for row in &mut manifest.rows {
        let Some(spec) = spec_for(&row.table) else {
            continue;
        };
        let mut values: Vec<(String, Option<String>)> = Vec::with_capacity(spec.columns.len());
        for column in spec.columns {
            let incoming = row
                .values
                .iter()
                .find(|(name, _)| name == column)
                .and_then(|(_, value)| value.clone());
            values.push((column.to_string(), incoming));
        }
        let dropped: Vec<String> = row
            .values
            .iter()
            .filter(|(name, _)| !spec.columns.iter().any(|column| *column == name.as_str()))
            .map(|(name, _)| name.clone())
            .collect();
        if !dropped.is_empty() {
            gaps.push(CognitionGap {
                kind: "migrated_legacy_column".into(),
                detail: format!(
                    "{} {} carried legacy columns {}",
                    row.table,
                    row.key,
                    dropped.join(", ")
                ),
                blocking: false,
            });
        }
        row.values = values;
    }
    gaps
}

/// `insert_row` uses positional values while admission and recovery use
/// labels. Bind those two views, and the primary-key identity, before any
/// publication. Both supported archive schemas use these same TableSpecs.
pub(super) fn validate_cognition_rows(manifest: &CognitionManifest) -> Result<()> {
    let mut identities = HashSet::with_capacity(manifest.rows.len());
    for row in &manifest.rows {
        let spec = spec_for(&row.table)
            .ok_or_else(|| anyhow!("unsupported cognition fact table {}", row.table))?;
        ensure!(row.values.len() == spec.columns.len()
            && row.values.iter().zip(spec.columns).all(|((name, _), expected)| name.as_str() == *expected),
            "invalid cognition column layout for {}", row.table);

        // Use the same encoding as read_table, without allocating another
        // joined key for every row. NULL has the exporter's empty encoding.
        let mut remainder = row.key.as_str();
        for (index, name) in spec.key.iter().enumerate() {
            if index != 0 {
                remainder = remainder.strip_prefix('\u{1f}')
                    .ok_or_else(|| anyhow!("cognition primary key mismatch for {}", row.table))?;
            }
            remainder = remainder.strip_prefix(row.value(name).unwrap_or_default())
                .ok_or_else(|| anyhow!("cognition primary key mismatch for {}", row.table))?;
        }
        ensure!(remainder.is_empty(), "cognition primary key mismatch for {}", row.table);
        ensure!(identities.insert((row.table.as_str(), row.key.as_str())),
            "duplicate cognition fact identity in {}", row.table);
    }
    crate::subject_branches::validate_branch_rows(&manifest.rows)?;
    super::root_facts::validate_root_facts(&manifest.rows)?;
    Ok(())
}

/// The REQUIRED-fact closure of an archive: every member row must find its
/// anchor row in the same archive. A gap here is blocking — the archive
/// does not describe a complete set of facts.
pub(super) fn archive_membership_gaps(rows: &[CognitionRow]) -> Vec<CognitionGap> {
    let mut gaps = Vec::new();
    for entry in FACT_CLASS_ANCHORS {
        let member = entry.0;
        let anchor = entry.1;
        let member_column = entry.2;
        let anchor_column = entry.3;
        let anchors: HashSet<&str> = rows
            .iter()
            .filter(|row| row.table == anchor)
            .filter_map(|row| row.value(anchor_column))
            .collect();
        for row in rows.iter().filter(|row| row.table == member) {
            let Some(binding) = row.value(member_column) else {
                continue;
            };
            if !anchors.contains(binding) {
                gaps.push(CognitionGap {
                    kind: "missing_anchor".into(),
                    detail: format!(
                        "{} {} has no {} for {}={}",
                        member, row.key, anchor, member_column, binding
                    ),
                    blocking: true,
                });
            }
        }
    }
    // A revision names the version it supersedes. Dropping that version would
    // silently turn a revision into an unrelated first version, so the parent
    // content has to travel too (the chain only grows).
    let archived_contents: HashSet<&str> = rows
        .iter()
        .filter(|row| row.table == "cognition_contents")
        .filter_map(|row| row.value("digest"))
        .collect();
    for row in rows
        .iter()
        .filter(|row| row.table == "cognition_attitudes")
    {
        let Some(parent) = row.value("supersedes_content") else {
            continue;
        };
        if !archived_contents.contains(parent) {
            gaps.push(CognitionGap {
                kind: "missing_anchor".into(),
                detail: format!(
                    "cognition attitude {} revises {}, which the archive does not carry",
                    row.key, parent
                ),
                blocking: true,
            });
        }
    }
    // A quota event names the fact it describes by id in its payload, and it
    // is written in the same transaction as that fact. The archive must
    // carry the cause, so an event never travels without it.
    for (field, cause, id_column) in QUOTA_EVENT_CAUSES {
        let present: HashSet<&str> = rows
            .iter()
            .filter(|row| row.table == *cause)
            .filter_map(|row| row.value(id_column))
            .collect();
        if present.is_empty() {
            continue;
        }
        for row in rows.iter().filter(|row| row.table == "quota_events") {
            let Some(data) = row.value("data") else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<serde_json::Value>(data) else {
                continue;
            };
            let Some(payload) = event.get("payload") else {
                continue;
            };
            let Some(id) = payload.get(*field).and_then(serde_json::Value::as_str) else {
                continue;
            };
            if !present.contains(id) {
                gaps.push(CognitionGap {
                    kind: "missing_cause".into(),
                    detail: format!(
                        "quota event {} has no {} for {}={}",
                        row.key, cause, field, id
                    ),
                    blocking: true,
                });
            }
        }
    }
    // An obligation and an attention policy name the sealed decision of the
    // SAME step that authored them, and an obligation's origin event must
    // be an event this archive knows. A row whose step or event is missing
    // is an incomplete fact, not a fact to interpret loosely.
    let steps: HashMap<&str, &CognitionRow> = rows
        .iter()
        .filter(|row| row.table == "subject_steps")
        .map(|row| (row.key.as_str(), row))
        .collect();
    for row in rows.iter().filter(|row| {
        row.table == "subject_obligations" || row.table == "subject_attention_policies"
    }) {
        let Some(step_id) = row.value("step_id") else {
            continue;
        };
        let Some(step) = steps.get(step_id) else {
            gaps.push(CognitionGap {
                kind: "missing_step".into(),
                detail: format!("{} {} has no subject step {}", row.table, row.key, step_id),
                blocking: true,
            });
            continue;
        };
        // validate_obligation_rows authenticates the original CAS decision,
        // its complete creation binding and any recorded publication migration.
        // A migrated step names the new object while the obligation must keep
        // its original creation reference; raw digest equality is not its rule.
        if row.table == "subject_obligations" {
            continue;
        }
        let step_decision = step
            .value("data")
            .and_then(|data| serde_json::from_str::<serde_json::Value>(data).ok())
            .and_then(|step| step.get("decision_ref").cloned())
            .and_then(|decision| {
                decision
                    .get("digest")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            });
        let row_decision = row.value("decision_ref").and_then(decision_ref_digest);
        if let (Some(step_decision), Some(row_decision)) = (step_decision, row_decision) {
            if step_decision != row_decision {
                gaps.push(CognitionGap {
                    kind: "decision_mismatch".into(),
                    detail: format!(
                        "{} {} names decision {} but its step {} decided {}",
                        row.table, row.key, row_decision, step_id, step_decision
                    ),
                    blocking: true,
                });
            }
        }
    }
    let events: HashSet<&str> = rows
        .iter()
        .filter(|row| row.table == "inbox_events")
        .map(|row| row.key.as_str())
        .collect();
    for row in rows.iter().filter(|row| row.table == "subject_obligations") {
        let Some(origin) = row.value("origin_event_id") else {
            continue;
        };
        if !events.contains(origin) {
            gaps.push(CognitionGap {
                kind: "missing_origin_event".into(),
                detail: format!("obligation {} names origin event {}", row.key, origin),
                blocking: true,
            });
        }
    }
    gaps
}

/// The content identity of a sealed decision reference.
fn decision_ref_digest(value: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(value).ok()?;
    parsed
        .get("digest")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

pub(super) fn memory_intent_binding_gaps(rows: &[CognitionRow]) -> Vec<CognitionGap> {
    let mut intents = HashMap::new();
    let mut bindings = HashMap::new();
    let mut entries = HashSet::new();
    let mut gaps = Vec::new();
    // The subject settles its own memory in one ledger transaction and
    // intentionally carries no library binding sidecar. That dedicated
    // representation is valid, not corrupt: it only needs its own facts.
    let subject_memory_intents: HashSet<&str> = rows
        .iter()
        .filter(|row| row.table == "subject_intents")
        .filter(|row| row.value("kind") == Some("memory"))
        .filter_map(|row| row.value("intent_id"))
        .collect();
    for row in rows.iter().filter(|row| row.table == "memory_entries") {
        if row.value("entry_id") != Some(row.key.as_str()) || !entries.insert(row.key.as_str()) {
            gaps.push(CognitionGap {
                kind: "memory_intent_identity".into(),
                detail: format!("invalid or duplicate memory entry {}", row.key),
                blocking: true,
            });
        }
    }
    for row in rows.iter().filter(|row| row.table == "memory_intents" || row.table == "memory_intent_bindings") {
        let id = row.value("intent_id");
        let map = if row.table == "memory_intents" { &mut intents } else { &mut bindings };
        if id != Some(row.key.as_str()) || map.insert(row.key.as_str(), row).is_some() {
            gaps.push(CognitionGap {
                kind: "memory_intent_identity".into(),
                detail: format!("invalid or duplicate {} row {}", row.table, row.key),
                blocking: true,
            });
        }
    }
    for (id, binding) in &bindings {
        let valid = intents.get(id).is_some_and(|intent| {
            binding.value("data").is_some_and(|data| {
                crate::memory::memory_binding_matches_locator(data, intent.value("content_ref"))
            })
        });
        if !valid {
            gaps.push(CognitionGap {
                kind: "memory_intent_identity".into(),
                detail: format!("unbound or inconsistent memory reference for {id}"),
                blocking: true,
            });
        }
    }
    let mut entry_owners = HashMap::new();
    for (id, intent) in &intents {
        let subject_owned = subject_memory_intents.contains(*id);
        let library_pending = !subject_owned
            && intent.value("state") == Some("accepted")
            && matches!(intent.value("kind"), Some("save" | "correct" | "forget" | "recall"));
        let entry_present = intent.value("entry_id").is_some_and(|id| id.parse::<EntityId>().is_ok());
        let entry_id = intent.value("entry_id");
        // Only producing intents own their entry: a forget *references* the
        // entry it retires, so a Save→Forget history legitimately binds the
        // same entry_id twice without sharing a produced identity.
        let produces_entry = intent.value("kind") != Some("forget");
        let duplicate_entry = produces_entry
            && entry_id.is_some_and(|entry| entry_owners.insert(entry, *id).is_some());
        let missing_committed_entry = intent.value("state") == Some("committed")
            && entry_id.is_none_or(|entry| !entries.contains(entry));
        let missing_pending_identity = library_pending
            && (!entry_present || (intent.value("content_ref").is_some() && !bindings.contains_key(id)));
        if missing_pending_identity || duplicate_entry || missing_committed_entry {
            gaps.push(CognitionGap {
                kind: "memory_intent_identity".into(),
                detail: if missing_pending_identity {
                    format!("pending memory intent {id} lacks its complete identity")
                } else {
                    format!("memory intent {id} has a missing or shared committed entry")
                },
                blocking: true,
            });
        }
    }
    gaps
}
