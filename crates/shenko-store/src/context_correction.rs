//! Admission checks for a correction's new projection observation. The parent
//! seal remains its own immutable observation; a newer window never relabels it.
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;
use shenko_types::{
    error::ShenkoError,
    object::{Digest, ObjectRef},
    ids::EntityId,
    subject::{ControlCorrectionDisposition, ControlInputReselection, ControlOutputRejection,
        ControlOutputReplacement, MAX_PINNED_CONTROL_CORRECTIONS, SubjectBranchHead, SubjectStep},
};

use crate::{Store, SubjectContextRef, store::storage_error};

fn invalid(reason: &str) -> ShenkoError {
    ShenkoError::conflict(format!("correction projection refused: {reason}"))
}

fn number(value: &Value, key: &str) -> Result<u64, ShenkoError> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("required projection count missing"))
}

fn messages(plan: &Value) -> Result<&Vec<Value>, ShenkoError> {
    plan.pointer("/payload/messages")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("frozen messages missing"))
}

fn partitions(plan: &Value) -> Result<(usize, usize, usize), ShenkoError> {
    let layout = plan
        .pointer("/context/payload_layout")
        .ok_or_else(|| invalid("schema-14 layout missing"))?;
    let prefix = usize::try_from(number(layout, "system_prefix_bytes")?)
        .map_err(|_| invalid("system prefix overflow"))?;
    let history = number(layout, "history_message_count")?;
    let idle = number(layout, "idle_message_count")?;
    let contribution = number(layout, "contribution_message_count")?;
    let tail = history
        .checked_add(idle)
        .and_then(|value| value.checked_add(contribution))
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| invalid("message partition overflow"))?;
    let history = usize::try_from(history).map_err(|_| invalid("history count overflow"))?;
    let system = plan
        .pointer("/payload/system_prompt")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("frozen system material missing"))?;
    if idle > 1 || !system.is_char_boundary(prefix) || tail > messages(plan)?.len() {
        return Err(invalid("invalid UTF-8 prefix or message partitions"));
    }
    Ok((prefix, history, tail))
}

fn sealed_json(store: &Store, reference: &ObjectRef) -> Result<Value, ShenkoError> {
    let bytes = store.cas.read_object(&reference.digest)?;
    if reference.locator.is_empty()
        || bytes.len() as u64 != reference.bytes
        || Digest::blake3(&bytes) != reference.digest
    {
        return Err(ShenkoError::ObjectClosureIncomplete { missing_count: 1 });
    }
    Ok(serde_json::from_slice(&bytes)?)
}

impl Store {
    /// Plan R0 using durable input and exact parent-seal provenance. The
    /// atomic replacement validates this observation again under its lock.
    pub fn control_output_correction(&self, head: &SubjectBranchHead, previous: &SubjectStep)
        -> Result<ControlCorrectionDisposition, ShenkoError> {
        let conn = self.ledger.conn.lock();
        crate::store::checked_head(&conn, head)?;
        self.control_output_correction_at(&conn, head, previous)
    }

    fn control_output_correction_at(&self, conn: &Connection, head: &SubjectBranchHead,
        previous: &SubjectStep) -> Result<ControlCorrectionDisposition, ShenkoError> {
        if !self.recorded_model_outcome(previous)?.permits_control_correction() {
            return Err(invalid("unknown/nonterminal model outcome cannot replace its step"));
        }
        let parent_ref = previous.input_plan_ref.as_ref().ok_or_else(|| invalid("parent seal missing"))?;
        let parent = sealed_json(self, parent_ref)?;
        let current_format = matches!(parent["schema_version"].as_u64(), Some(14 | 15));
        let mut observed = parent.clone();
        let mut prior_pinned_corrections = 0;
        while prior_pinned_corrections < MAX_PINNED_CONTROL_CORRECTIONS {
            let Some(reference) = observed.pointer("/context/correction_observation/parent_input_ref") else { break; };
            let reference: ObjectRef = serde_json::from_value(reference.clone())?;
            observed = sealed_json(self, &reference)?;
            prior_pinned_corrections += 1;
        }
        let selected: Vec<EntityId> = parent.get("events").and_then(Value::as_array)
            .ok_or_else(|| invalid("selected input identities missing"))?.iter()
            .map(|event| serde_json::from_value(event["event_id"].clone()).map_err(ShenkoError::from))
            .collect::<Result<_, _>>()?;
        // Kind/source columns are bound to complete ingress at admission.
        // Respect the same source pause and settled-event boundary as S0;
        // message text itself never gets to claim interactive priority.
        let fresh: Option<String> = conn.query_row(
            "WITH attention AS (
                 SELECT state,sources,review_after FROM subject_attention_policies
                 WHERE agent_id=?1 ORDER BY created_seq DESC,intent_id DESC LIMIT 1
             )
             SELECT i.event_id FROM inbox_events i
             WHERE i.agent_id=?1 AND i.consumed=0 AND i.kind IN ('message','media_priority')
               AND NOT EXISTS (SELECT 1 FROM subject_event_dispositions d
                   WHERE d.branch_id=?2 AND d.event_id=i.event_id)
               AND NOT EXISTS (SELECT 1 FROM json_each(?3) selected WHERE selected.value=i.event_id)
               AND NOT EXISTS (SELECT 1 FROM attention a WHERE a.state='\"off\"'
                   AND (a.review_after IS NULL OR julianday(a.review_after)>julianday(?4))
                   AND (json_array_length(a.sources)=0 OR EXISTS (
                       SELECT 1 FROM json_each(a.sources) source WHERE source.value=i.source)))
             ORDER BY i.rowid LIMIT 1",
            params![head.agent_id.to_string(), head.branch_id.0.to_string(),
                serde_json::to_string(&selected)?, chrono::Utc::now().to_rfc3339()],
            |row| row.get(0)).optional().map_err(storage_error)?;
        let replacement = if let Some(id) = fresh {
            ControlOutputReplacement::ReselectInput { cause: ControlInputReselection::FreshInteractive {
                event_id: id.parse().map_err(storage_error)?,
            } }
        } else if prior_pinned_corrections >= MAX_PINNED_CONTROL_CORRECTIONS {
            ControlOutputReplacement::ReselectInput { cause: ControlInputReselection::CorrectionLimit }
        } else if !current_format {
            ControlOutputReplacement::ReselectInput { cause: ControlInputReselection::LegacyProjection }
        } else { ControlOutputReplacement::PinnedCorrection };
        Ok(ControlCorrectionDisposition { prior_pinned_corrections, replacement })
    }

    pub(crate) fn requeue_rejected_inputs(&self, tx: &Transaction<'_>,
        head: &SubjectBranchHead, previous: &SubjectStep) -> Result<(), ShenkoError> {
        let parent = sealed_json(self, previous.input_plan_ref.as_ref()
            .ok_or_else(|| invalid("reselected parent seal missing"))?)?;
        let selected = parent.get("events").and_then(Value::as_array)
            .ok_or_else(|| invalid("reselected event identities missing"))?;
        // Staging's consumed flag is only a delivery checkpoint, never an
        // acknowledgement. Re-open it atomically when its owner is retired;
        // already terminal dispositions remain terminal and raw is unchanged.
        tx.execute("UPDATE inbox_events SET consumed=0
            WHERE agent_id=?1 AND event_id IN (
                SELECT json_extract(value,'$.event_id') FROM json_each(?3))
              AND NOT EXISTS (SELECT 1 FROM subject_event_dispositions d
                WHERE d.branch_id=?2 AND d.event_id=inbox_events.event_id
                  AND d.disposition!='\"deferred\"'
                  AND COALESCE(json_extract(d.disposition,'$.disposition'),'')!='deferred')",
            params![head.agent_id.to_string(), head.branch_id.0.to_string(), serde_json::to_string(selected)?])
            .map_err(storage_error)?;
        Ok(())
    }

    pub(crate) fn validate_correction_projection(
        &self,
        tx: &Transaction<'_>,
        head: &SubjectBranchHead,
        previous: &SubjectStep,
        replacement: &SubjectStep,
        rejection: &ControlOutputRejection,
    ) -> Result<(), ShenkoError> {
        if rejection.schema_version == 3 {
            let observed = self.control_output_correction_at(tx, head, previous)?;
            if rejection.correction.as_ref() != Some(&observed) {
                return Err(ShenkoError::ResnapshotRequired {
                    reason: "control correction input/attempt observation changed".into(),
                });
            }
            if matches!(observed.replacement, ControlOutputReplacement::ReselectInput { .. }) {
                // No new S0 exists yet. The old raw/input/outcome remains
                // immutable, and the ordinary Planned path will select it
                // together with newly admitted input under a new request.
                return Ok(());
            }
        }
        let next_ref = replacement
            .input_plan_ref
            .as_ref()
            .ok_or_else(|| invalid("replacement seal missing"))?;
        let next = sealed_json(self, next_ref)?;
        let parent_ref = previous
            .input_plan_ref
            .as_ref()
            .ok_or_else(|| invalid("parent seal missing"))?;
        let parent = sealed_json(self, parent_ref)?;
        let version = next.get("schema_version").and_then(Value::as_u64);
        if !crate::store::admitted_input_plan_schema(version)
            || version != parent.get("schema_version").and_then(Value::as_u64)
        {
            return Err(invalid(
                "correction cannot downgrade or reinterpret its parent's schema",
            ));
        }
        if !matches!(version, Some(14 | 15)) {
            return Ok(());
        }
        if [next_ref, parent_ref].iter().any(|reference| {
            reference.store_namespace != "input_plans"
                || reference.codec.name != "subject_step_input"
                || Some(u64::from(reference.codec.major)) != version
        }) || next.get("step_id") != Some(&serde_json::to_value(replacement.step_id)?)
            || next.get("step_no").and_then(Value::as_u64) != Some(replacement.step_no)
            || next.get("request_id") == parent.get("request_id")
        {
            return Err(invalid("schema/request/step identity differs"));
        }
        let _: shenko_types::ids::EntityId = serde_json::from_value(
            next.get("request_id")
                .cloned()
                .ok_or_else(|| invalid("replacement request identity missing"))?,
        )?;
        let observation = next
            .pointer("/context/correction_observation")
            .ok_or_else(|| invalid("correction observation missing"))?;
        let old_source: SubjectContextRef = serde_json::from_value(
            parent
                .pointer("/context/source")
                .cloned()
                .ok_or_else(|| invalid("parent source missing"))?,
        )?;
        let new_source: SubjectContextRef = serde_json::from_value(
            next.pointer("/context/source")
                .cloned()
                .ok_or_else(|| invalid("new source missing"))?,
        )?;
        if observation.get("parent_input_ref") != Some(&serde_json::to_value(parent_ref)?)
            || observation.get("parent_source") != parent.pointer("/context/source")
            || old_source.scope_id != shenko_types::ids::EntityId(head.branch_id.0)
            || new_source.scope_id != old_source.scope_id
            || new_source.leaf_seq != old_source.leaf_seq
            || new_source.input_raw_event_ids != old_source.input_raw_event_ids
            || new_source.model_generation != old_source.model_generation
            || new_source.window_generation < old_source.window_generation
        {
            return Err(invalid("parent seal/raw scope boundary changed"));
        }
        for field in [
            "events",
            "provider_binding",
            "child_provider_bindings",
            "provider_selection",
            "identity",
            "identity_provider_revision",
            "hook_graph_ref",
            "contributions",
            "contribution_omissions",
            "contribution_tokens",
            "obligations",
            "reconciliation_docket",
            "attention",
            "semantic_wait",
        ] {
            if next.get(field) != parent.get(field) {
                return Err(invalid("pinned input material changed"));
            }
        }
        for field in ["system_prompt", "max_tokens", "tools", "native_options"] {
            let pointer = format!("/payload/{field}");
            if next.pointer(&pointer) != parent.pointer(&pointer) {
                return Err(invalid("pinned wire material changed"));
            }
        }
        for field in [
            "profile_digest",
            "capabilities",
            "reserved_output_tokens",
            "token_counter",
            "policy_contract",
        ] {
            let pointer = format!("/context/{field}");
            if next.pointer(&pointer) != parent.pointer(&pointer) {
                return Err(invalid("pinned context binding changed"));
            }
        }
        let (parent_prefix, parent_history, parent_tail) = partitions(&parent)?;
        let (next_prefix, next_history, next_tail) = partitions(&next)?;
        let parent_messages = messages(&parent)?;
        let next_messages = messages(&next)?;
        if parent_prefix != next_prefix
            || parent_messages[parent_history..parent_tail]
                != next_messages[next_history..next_tail]
        {
            return Err(invalid("original idle/contribution material changed"));
        }
        for field in ["idle_message_count", "contribution_message_count"] {
            let pointer = format!("/context/payload_layout/{field}");
            if next.pointer(&pointer) != parent.pointer(&pointer) {
                return Err(invalid("retained message boundary changed"));
            }
        }
        let output_ref = previous
            .model_result_ref
            .as_ref()
            .ok_or_else(|| invalid("parent model text missing"))?;
        let output = self.cas.read_object(&output_ref.digest)?;
        if output.len() as u64 != output_ref.bytes || Digest::blake3(&output) != output_ref.digest {
            return Err(ShenkoError::ObjectClosureIncomplete { missing_count: 1 });
        }
        let output = String::from_utf8(output).map_err(storage_error)?;
        let mut tail = parent_messages[parent_tail..].to_vec();
        tail.push(serde_json::json!({"role":"assistant","parts":[{"type":"text","text":output}]}));
        tail.push(serde_json::json!({"role":"user","parts":[{"type":"text","text":crate::control_rejection_feedback(rejection)}]}));
        if next_messages[next_tail..] != tail
            || observation
                .get("tail_message_count")
                .and_then(Value::as_u64)
                != Some(tail.len() as u64)
            || observation.get("tail_digest")
                != Some(&serde_json::to_value(Digest::blake3(&serde_json::to_vec(
                    &tail,
                )?))?)
        {
            return Err(invalid(
                "correction tail is not the complete original text and feedback",
            ));
        }
        let context = next
            .get("context")
            .ok_or_else(|| invalid("context missing"))?;
        let estimate = number(context, "estimated_input_tokens")?;
        let budget = number(context, "budget_input_tokens")?;
        let output = number(context, "reserved_output_tokens")?;
        if budget < estimate
            || next.pointer("/payload/max_tokens").and_then(Value::as_u64) != Some(output)
        {
            return Err(invalid(
                "final estimate/charged budget/output reservation disagree",
            ));
        }
        if let Some(limit) = next
            .pointer("/context/capabilities/max_context_tokens")
            .and_then(Value::as_u64)
        {
            if limit == 0 || budget.checked_add(output).is_none_or(|total| total > limit) {
                return Err(invalid(
                    "corrected request exceeds the served context limit",
                ));
            }
            let has_image = next_messages
                .iter()
                .filter_map(|message| message.get("parts").and_then(Value::as_array))
                .flatten()
                .any(|part| part.get("type").and_then(Value::as_str) == Some("image"));
            if has_image
                && next
                    .pointer("/context/capabilities/vision/max_image_context")
                    .and_then(Value::as_u64)
                    .is_none_or(|limit| {
                        budget.checked_add(output).is_none_or(|total| total > limit)
                    })
            {
                return Err(invalid(
                    "corrected image request exceeds the observed vision envelope",
                ));
            }
        }
        match observation.get("new_execution_projection") {
            Some(Value::Null) => {
                if next.pointer("/context/source") != parent.pointer("/context/source")
                    || parent_messages[..parent_history] != next_messages[..next_history]
                    || next.pointer("/context/unfinished_call_ids")
                        != parent.pointer("/context/unfinished_call_ids")
                    || next.pointer("/context/payload_layout/vision_tokens")
                        != parent.pointer("/context/payload_layout/vision_tokens")
                {
                    return Err(invalid(
                        "retained projection reinterprets the parent's observation",
                    ));
                }
            }
            Some(value) if Some(value) == next.pointer("/context/source") => {
                let current_seq: i64 = tx
                    .query_row("SELECT COALESCE(MAX(seq),0) FROM commit_log", [], |row| {
                        row.get(0)
                    })
                    .map_err(storage_error)?;
                let profile: Option<(i64, Option<String>)> = tx.query_row(
                    "SELECT model_generation,profile_digest FROM context_window_scopes WHERE scope_id=?1",
                    params![new_source.scope_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?)),
                ).optional().map_err(storage_error)?;
                let window: Option<(String, i64, String)> = tx.query_row(
                    "SELECT window_id,generation,body FROM context_windows WHERE scope_id=?1 ORDER BY generation DESC LIMIT 1",
                    params![new_source.scope_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                ).optional().map_err(storage_error)?;
                let digest = next
                    .pointer("/context/profile_digest")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("profile digest missing"))?;
                if u64::try_from(current_seq).ok() != Some(new_source.source_commit_seq)
                    || profile
                        != Some((
                            i64::try_from(new_source.model_generation).map_err(storage_error)?,
                            Some(digest.into()),
                        ))
                    || window.as_ref().is_none_or(|(id, generation, _)| {
                        id != &new_source.window_id.to_string()
                            || u64::try_from(*generation).ok() != Some(new_source.window_generation)
                    })
                {
                    return Err(ShenkoError::ResnapshotRequired {
                        reason: "correction execution observation/window/model moved before R0; original result retained".into(),
                    });
                }
                let (_, _, window_body) =
                    window.ok_or_else(|| invalid("current window missing"))?;
                let window_body: Value = serde_json::from_str(&window_body)?;
                if window_body.get("id") != Some(&serde_json::to_value(new_source.window_id)?)
                    || window_body.get("generation").and_then(Value::as_u64)
                        != Some(new_source.window_generation)
                    || window_body.get("model_generation").and_then(Value::as_u64)
                        != Some(new_source.model_generation)
                    || window_body
                        .get("covers_through")
                        .and_then(Value::as_u64)
                        .is_some_and(|through| {
                            new_source.leaf_seq.is_none_or(|leaf| through > leaf)
                        })
                {
                    return Err(invalid(
                        "current window differs from the original raw/model boundary",
                    ));
                }
            }
            _ => return Err(invalid("new execution observation is not explicitly bound")),
        }
        Ok(())
    }
}
