//! Current column contracts. A `_ref` spelling alone is not a storage type:
//! quota scope and historical memory locators are strings, while research
//! object columns and committed decision columns contain complete ObjectRefs.
use serde::de::DeserializeOwned;
use serde_json::Value;
use shenko_types::{authority::CommitAdmission, commit::CommitRef};

use super::RetentionOrigin;

pub(super) fn object_column(table: &str, column: &str) -> bool {
    matches!((table, column),
        ("research_epochs", "constraints_ref" | "budget_ref" | "effects_ref" | "environment_ref" | "brief_ref")
        | ("research_candidates", "artifact_ref" | "environment_ref")
        | ("subject_intents" | "subject_obligations" | "subject_attention_policies", "decision_ref")
        | ("subject_request_attempts", "input_plan_ref")
        | ("control_prepared_transitions", "transition_ref")
        | ("cognition_contents", "object_json")
        | ("recovery_sources", "data"))
}

pub(super) fn required_json(table: &str, column: &str) -> bool {
    object_column(table, column)
        || (column == "data" && !matches!(table, "raw_sessions" | "session_events" | "content_stream_units"))
        || column.ends_with("_json")
        || matches!((table, column),
            ("identity_profiles", "profile") | ("context_windows", "body")
            | ("cognition_baselines", "row_values")
            | ("control_prepared_transitions", "admission" | "result_commit")
            | ("subject_intents", "payload") | ("subject_attention_policies", "sources")
            | ("channel_runtime_facts", "detail")
            | ("research_epochs", "reset_receipt")
            | ("intervention_bank_entries", "source_refs")
            | ("intervention_reminders", "provenance" | "bank_entry_ids")
            | ("retention_tombstones", "effect_record")
            | ("content_streams", "opened_commit_ref" | "closed_commit_ref")
            | ("media_streams", "open_commit_ref")
            | ("media_stream_ends" | "media_segment_revisions" | "media_recordings"
                | "media_recording_controls" | "media_priority_inputs", "admitted_commit_ref"))
}

pub(super) fn required_digest(table: &str, column: &str) -> bool {
    matches!((table, column),
        ("cognition_contents", "digest")
        | ("cognition_attitudes", "digest" | "supersedes_content")
        | ("cognition_legacy_content", "content_digest")
        | ("cognitive_root_nodes" | "cognitive_root_requests", "content_digest"))
}

fn readable<T: DeserializeOwned>(value: &Value) -> bool {
    T::deserialize(value).is_ok()
}

fn version_one<T: DeserializeOwned>(value: &Value) -> bool {
    value.get("schema_version").and_then(Value::as_u64) == Some(1) && readable::<T>(value)
}

/// These root-bearing bookkeeping rows are not arbitrary raw events. Decode
/// their current shape without invoking admission/recovery or rewriting a row.
/// The generic walker still retains every visible edge, even after a refusal.
pub(super) fn readable_row(value: &Value, origin: &RetentionOrigin) -> bool {
    let RetentionOrigin::Row { table, column, .. } = origin else { return true; };
    if object_column(table, column) {
        return value.as_object().and_then(super::closure::object_reference).is_some();
    }
    match (table.as_str(), column.as_str()) {
        ("control_prepared_transitions", "admission") => version_one::<CommitAdmission>(value),
        ("control_prepared_transitions", "result_commit") => readable::<CommitRef>(value),
        ("native_evidence_materializations", "data") =>
            version_one::<crate::native_evidence::EvidenceMaterialization>(value),
        ("native_evidence_deliveries", "data") =>
            version_one::<crate::native_evidence_delivery::NativeEvidenceDeliveryRecord>(value),
        ("native_evidence_seal_debts", "data") =>
            version_one::<crate::native_evidence::NativeEvidenceSealDebt>(value),
        ("local_model_health", "data") =>
            version_one::<crate::local_model_health::LocalModelHealthHead>(value),
        ("local_installation_provisioning", "data") =>
            version_one::<crate::local_installation::LocalInstallationProvisioning>(value),
        ("reconciliation_cases", "data") => readable::<shenko_types::evidence::ReconciliationCase>(value),
        ("tasks", "data") => readable::<shenko_types::task::TaskRecord>(value),
        ("operations", "data") => readable::<shenko_types::operation::OperationRecord>(value),
        ("attempts", "data") => readable::<shenko_types::operation::AttemptRecord>(value),
        ("evidence_receipts", "data") => readable::<shenko_types::evidence::EvidenceReceipt>(value),
        ("control_materializations", "data") => {
            version_one::<CommitAdmission>(&value["admission"])
                && readable::<CommitRef>(&value["committed_at"])
                && value.get("logical_result").is_none_or(readable::<CommitRef>)
                && value.get("transition_ref").is_none_or(|reference|
                    reference.as_object().and_then(super::closure::object_reference).is_some())
        }
        ("cognition_baselines", "row_values") => readable::<Vec<(String, Option<String>)>>(value),
        _ => true,
    }
}
